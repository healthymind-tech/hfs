//! Dependency resolution: turns "install `name#version`" into an install
//! plan — the package tree, what happens to each node, and the order to
//! install in (dependencies first).
//!
//! The resolver only sees manifests through [`PackageSource`] (the registry
//! client plus the local cache in production, a map in tests) and the list
//! of installed packages; it never touches storage.
//!
//! Rules:
//! - `hl7.fhir.<release>.core` for the server's FHIR release maps to the
//!   definitions embedded in the server ([`NodeAction::Embedded`]); never
//!   imported (importing r4.core StructureDefinitions breaks the validator).
//! - The core packages' satellites (`.examples`, `.expansions`, `.elements`,
//!   `.search`) and other releases' core packages are skipped.
//! - Under [`DependencyPolicy::Newer`] (default) a requirement is met by an
//!   installed or already planned version at or above it, and when the tree
//!   needs several versions of one package the highest is planned once.
//! - In upgrade mode a planned package replaces the installed versions of
//!   the same name ([`NodeAction::Upgrade`]); in side-by-side mode they stay.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::tarball::Manifest;
use super::version::{DependencyPolicy, Range, compare};

/// Versions a source knows for one package.
#[derive(Debug, Clone, Default)]
pub struct VersionList {
    /// All known versions.
    pub versions: Vec<String>,
    /// The `latest` dist-tag, when known.
    pub latest: Option<String>,
}

/// Where manifests come from.
#[async_trait]
pub trait PackageSource: Send + Sync {
    /// Known versions of `name`.
    async fn versions(&self, name: &str) -> Result<VersionList, String>;
    /// The manifest of `name#version`.
    async fn manifest(&self, name: &str, version: &str) -> Result<Manifest, String>;
}

/// What happens when an already installed version exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallMode {
    /// The new version replaces installed versions of the same package.
    #[default]
    Upgrade,
    /// Installed versions stay; resources whose id is taken by another
    /// version get a version-suffixed id.
    SideBySide,
}

/// A resolution request.
#[derive(Debug, Clone, Default)]
pub struct PlanRequest {
    /// Package to install.
    pub name: String,
    /// Version; `None` means the latest.
    pub version: Option<String>,
    /// Upgrade or side-by-side.
    pub mode: InstallMode,
    /// How requirements are matched.
    pub policy: DependencyPolicy,
    /// Install dependencies at all.
    pub include_dependencies: bool,
    /// Dependency names the operator chose not to install.
    pub skip: BTreeSet<String>,
    /// The server's FHIR release (`r4`, `r4b`, `r5`, `r6`) for core mapping.
    pub fhir_release: String,
}

/// The fate of one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NodeAction {
    /// Install a package that is not installed in any version.
    Install,
    /// The same version is installed already; install it again.
    Reinstall,
    /// Install, then remove the listed installed versions.
    Upgrade {
        /// Versions replaced.
        from: Vec<String>,
    },
    /// Already met by an installed package (`name#version`).
    Satisfied {
        /// The installed package that meets the requirement.
        by: String,
    },
    /// The server's embedded core definitions.
    Embedded,
    /// Not installed, with the reason.
    Skipped {
        /// Why.
        reason: String,
    },
    /// The package could not be found.
    Missing {
        /// The lookup error.
        error: String,
    },
}

impl NodeAction {
    /// Whether the node will be installed by this plan.
    pub fn installs(&self) -> bool {
        matches!(self, Self::Install | Self::Reinstall | Self::Upgrade { .. })
    }
}

/// One package in the tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanNode {
    /// Package name.
    pub name: String,
    /// Resolved version (absent for missing packages without one).
    pub version: Option<String>,
    /// The requirements that led here (`range` from `parent`).
    pub required: Vec<Requirement>,
    /// What happens to it.
    pub action: NodeAction,
    /// Distance from the root (root = 0).
    pub depth: usize,
    /// Child package names, as listed in the manifest.
    pub dependencies: Vec<String>,
    /// Manifest title, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Manifest FHIR versions, when known.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub fhir_versions: Vec<String>,
}

impl PlanNode {
    /// `name#version` (or just the name when there is no version).
    pub fn key(&self) -> String {
        match &self.version {
            Some(v) => format!("{}#{v}", self.name),
            None => self.name.clone(),
        }
    }
}

/// One edge of the tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    /// The requiring package (`name#version`).
    pub parent: String,
    /// The range it asked for.
    pub range: String,
}

/// A resolved plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// The requested package, `name#version`.
    pub root: String,
    /// Every node, root first, then in discovery order.
    pub nodes: Vec<PlanNode>,
    /// Packages to install, dependencies before dependents (`name#version`).
    pub install_order: Vec<String>,
    /// Non-fatal notes (missing dependencies, unsupported FHIR versions).
    pub warnings: Vec<String>,
}

impl Plan {
    /// The node for `name#version`.
    pub fn node(&self, key: &str) -> Option<&PlanNode> {
        self.nodes.iter().find(|n| n.key() == key)
    }
}

/// Whether `name` is a core package (any release) and which release.
fn core_release(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("hl7.fhir.")?;
    let (release, part) = rest.split_once('.')?;
    if matches!(release, "r2" | "r3" | "r4" | "r4b" | "r5" | "r6") {
        Some((release, part))
    } else {
        None
    }
}

/// Classifies a dependency that is never installed from its package.
fn special(name: &str, fhir_release: &str) -> Option<NodeAction> {
    let (release, part) = core_release(name)?;
    match (release == fhir_release, part) {
        (true, "core") => Some(NodeAction::Embedded),
        (true, "examples" | "expansions" | "elements" | "search") => Some(NodeAction::Skipped {
            reason: "FHIR core examples/expansions are not loaded; the server embeds the core \
                     definitions"
                .into(),
        }),
        (false, _) => Some(NodeAction::Skipped {
            reason: format!("{name} is for another FHIR release than this server ({fhir_release})"),
        }),
        _ => None,
    }
}

/// Resolves a requirement to a concrete version using the source's list.
async fn resolve_range(
    source: &dyn PackageSource,
    name: &str,
    range: &str,
) -> Result<String, String> {
    let parsed = Range::parse(range);
    if let Range::Exact(v) = &parsed {
        return Ok(v.clone());
    }
    let list = source.versions(name).await?;
    parsed
        .pick(&list.versions, list.latest.as_deref())
        .map(str::to_string)
        .ok_or_else(|| format!("no version of {name} matches '{range}'"))
}

struct Traversal {
    nodes: Vec<PlanNode>,
    index: HashMap<String, usize>,
    edges: Vec<(String, String)>,
    /// To-install requirements per package name (Newer policy bumping).
    wanted: BTreeMap<String, Vec<String>>,
    warnings: Vec<String>,
}

/// Builds the plan.
pub async fn resolve(
    request: &PlanRequest,
    installed: &[(String, String)],
    source: &dyn PackageSource,
) -> Result<Plan, String> {
    let root_version = match &request.version {
        Some(v) => v.clone(),
        None => resolve_range(source, &request.name, "latest").await?,
    };
    let mut chosen: BTreeMap<String, String> = BTreeMap::new();
    let mut traversal = traverse(request, &root_version, installed, source, &chosen).await?;
    if request.policy == DependencyPolicy::Newer {
        for _ in 0..8 {
            let mut changed = false;
            for (name, versions) in &traversal.wanted {
                let Some(best) = versions.iter().max_by(|a, b| compare(a, b)) else {
                    continue;
                };
                let current = chosen.get(name);
                if current.is_none_or(|c| compare(best, c) == std::cmp::Ordering::Greater) {
                    chosen.insert(name.clone(), best.clone());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
            traversal = traverse(request, &root_version, installed, source, &chosen).await?;
        }
    }

    // Post-order over install edges: dependencies first.
    let root_key = format!("{}#{root_version}", request.name);
    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    let mut stack = vec![(root_key.clone(), false)];
    while let Some((key, expanded)) = stack.pop() {
        if expanded {
            if traversal
                .index
                .get(&key)
                .is_some_and(|i| traversal.nodes[*i].action.installs())
                && !order.contains(&key)
            {
                order.push(key);
            }
            continue;
        }
        if !seen.insert(key.clone()) {
            continue;
        }
        stack.push((key.clone(), true));
        let children: Vec<String> = traversal
            .edges
            .iter()
            .filter(|(parent, _)| *parent == key)
            .map(|(_, child)| child.clone())
            .collect();
        for child in children.into_iter().rev() {
            if !seen.contains(&child) {
                stack.push((child, false));
            }
        }
    }

    Ok(Plan {
        root: root_key,
        nodes: traversal.nodes,
        install_order: order,
        warnings: traversal.warnings,
    })
}

async fn traverse(
    request: &PlanRequest,
    root_version: &str,
    installed: &[(String, String)],
    source: &dyn PackageSource,
    chosen: &BTreeMap<String, String>,
) -> Result<Traversal, String> {
    let mut t = Traversal {
        nodes: Vec::new(),
        index: HashMap::new(),
        edges: Vec::new(),
        wanted: BTreeMap::new(),
        warnings: Vec::new(),
    };

    // Root.
    let root_manifest = source
        .manifest(&request.name, root_version)
        .await
        .map_err(|e| format!("{}#{root_version}: {e}", request.name))?;
    let others = installed_versions(installed, &request.name);
    let root_action = if others.iter().any(|v| v == root_version) {
        NodeAction::Reinstall
    } else if !others.is_empty() && request.mode == InstallMode::Upgrade {
        NodeAction::Upgrade { from: others }
    } else {
        NodeAction::Install
    };
    let root_key = format!("{}#{root_version}", request.name);
    push_node(
        &mut t,
        PlanNode {
            name: request.name.clone(),
            version: Some(root_version.to_string()),
            required: Vec::new(),
            action: root_action,
            depth: 0,
            dependencies: root_manifest.dependencies.keys().cloned().collect(),
            title: root_manifest.title.clone(),
            fhir_versions: root_manifest.fhir_versions.clone(),
        },
    );

    let mut queue: VecDeque<(String, Manifest, usize)> = VecDeque::new();
    queue.push_back((root_key, root_manifest, 0));
    while let Some((parent_key, manifest, depth)) = queue.pop_front() {
        for (dep, range) in &manifest.dependencies {
            let requirement = Requirement {
                parent: parent_key.clone(),
                range: range.clone(),
            };
            let depth = depth + 1;
            // Never-installed classes first.
            let fixed = if let Some(action) = special(dep, &request.fhir_release) {
                Some(action)
            } else if request.skip.contains(dep) {
                Some(NodeAction::Skipped {
                    reason: "excluded by the operator".into(),
                })
            } else if !request.include_dependencies {
                Some(NodeAction::Skipped {
                    reason: "dependencies were not requested".into(),
                })
            } else {
                None
            };
            if let Some(action) = fixed {
                let key = dep.clone();
                attach(&mut t, &parent_key, &key, requirement, || PlanNode {
                    name: dep.clone(),
                    // Keyed by name: the required range is on the edge.
                    version: None,
                    required: Vec::new(),
                    action,
                    depth,
                    dependencies: Vec::new(),
                    title: None,
                    fhir_versions: Vec::new(),
                });
                continue;
            }
            let parsed = Range::parse(range);
            let have = installed_versions(installed, dep);
            // Installed versions that meet the requirement (best first).
            let mut meeting: Vec<&String> = have
                .iter()
                .filter(|v| parsed.satisfied_by(v, request.policy))
                .collect();
            meeting.sort_by(|a, b| compare(b, a));
            let planned = chosen.get(dep);
            // A planned version that is not installed yet takes precedence
            // over an installed one: some other requirement needs it, and
            // every edge should point at the version actually installed.
            let planned_elsewhere = planned
                .is_some_and(|p| parsed.satisfied_by(p, request.policy) && !have.contains(p));
            if let (Some(best), false) = (meeting.first(), planned_elsewhere) {
                let key = format!("{dep}#{best}");
                attach(&mut t, &parent_key, &key, requirement, || PlanNode {
                    name: dep.clone(),
                    version: Some((*best).clone()),
                    required: Vec::new(),
                    action: NodeAction::Satisfied { by: key.clone() },
                    depth,
                    dependencies: Vec::new(),
                    title: None,
                    fhir_versions: Vec::new(),
                });
                continue;
            }
            // To install: the version this requirement asks for, unless the
            // Newer policy already chose a higher one for the name.
            let own = match resolve_range(source, dep, range).await {
                Ok(v) => v,
                Err(error) => {
                    t.warnings.push(format!("{dep} ({range}): {error}"));
                    attach(&mut t, &parent_key, dep, requirement, || PlanNode {
                        name: dep.clone(),
                        version: None,
                        required: Vec::new(),
                        action: NodeAction::Missing { error },
                        depth,
                        dependencies: Vec::new(),
                        title: None,
                        fhir_versions: Vec::new(),
                    });
                    continue;
                }
            };
            let version = match (request.policy, planned) {
                (DependencyPolicy::Newer, Some(p)) if compare(p, &own).is_ge() => p.clone(),
                _ => own.clone(),
            };
            if request.policy == DependencyPolicy::Newer {
                t.wanted.entry(dep.clone()).or_default().push(own);
            }
            let key = format!("{dep}#{version}");
            if t.index.contains_key(&key) {
                attach(&mut t, &parent_key, &key, requirement, || {
                    unreachable_node()
                });
                continue;
            }
            let manifest = match source.manifest(dep, &version).await {
                Ok(m) => m,
                Err(error) => {
                    t.warnings.push(format!("{key}: {error}"));
                    attach(&mut t, &parent_key, &key, requirement, || PlanNode {
                        name: dep.clone(),
                        version: Some(version.clone()),
                        required: Vec::new(),
                        action: NodeAction::Missing { error },
                        depth,
                        dependencies: Vec::new(),
                        title: None,
                        fhir_versions: Vec::new(),
                    });
                    continue;
                }
            };
            let lower: Vec<String> = have
                .iter()
                .filter(|v| compare(v, &version).is_lt())
                .cloned()
                .collect();
            let action = if have.iter().any(|v| compare(v, &version).is_eq()) {
                NodeAction::Reinstall
            } else if request.mode == InstallMode::Upgrade
                && request.policy == DependencyPolicy::Newer
                && !lower.is_empty()
            {
                NodeAction::Upgrade { from: lower }
            } else {
                NodeAction::Install
            };
            let node = PlanNode {
                name: dep.clone(),
                version: Some(version.clone()),
                required: Vec::new(),
                action,
                depth,
                dependencies: manifest.dependencies.keys().cloned().collect(),
                title: manifest.title.clone(),
                fhir_versions: manifest.fhir_versions.clone(),
            };
            attach(&mut t, &parent_key, &key, requirement, || node);
            queue.push_back((key, manifest, depth));
        }
    }
    Ok(t)
}

fn installed_versions(installed: &[(String, String)], name: &str) -> Vec<String> {
    installed
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, v)| v.clone())
        .collect()
}

fn unreachable_node() -> PlanNode {
    // `attach` only calls the constructor for an unseen key.
    PlanNode {
        name: String::new(),
        version: None,
        required: Vec::new(),
        action: NodeAction::Embedded,
        depth: 0,
        dependencies: Vec::new(),
        title: None,
        fhir_versions: Vec::new(),
    }
}

fn push_node(t: &mut Traversal, node: PlanNode) {
    let key = node.key();
    t.index.insert(key, t.nodes.len());
    t.nodes.push(node);
}

/// Records `parent -> key`, creating the node on first sight.
fn attach(
    t: &mut Traversal,
    parent: &str,
    key: &str,
    requirement: Requirement,
    make: impl FnOnce() -> PlanNode,
) {
    let index = match t.index.get(key) {
        Some(i) => *i,
        None => {
            let node = make();
            let i = t.nodes.len();
            t.index.insert(key.to_string(), i);
            t.nodes.push(node);
            i
        }
    };
    t.nodes[index].required.push(requirement);
    if !t.edges.iter().any(|(p, c)| p == parent && c == key) {
        t.edges.push((parent.to_string(), key.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// `name -> [(version, [(dependency, range)])]`.
    type Registry = HashMap<String, Vec<(String, Vec<(String, String)>)>>;

    /// A registry in a map.
    struct MapSource(Registry);

    impl MapSource {
        fn new(entries: &[(&str, &str, &[(&str, &str)])]) -> Self {
            let mut map: Registry = HashMap::new();
            for (name, version, deps) in entries {
                map.entry(name.to_string()).or_default().push((
                    version.to_string(),
                    deps.iter()
                        .map(|(n, v)| (n.to_string(), v.to_string()))
                        .collect(),
                ));
            }
            Self(map)
        }
    }

    #[async_trait]
    impl PackageSource for MapSource {
        async fn versions(&self, name: &str) -> Result<VersionList, String> {
            let list = self
                .0
                .get(name)
                .ok_or_else(|| format!("{name} not found"))?;
            let versions: Vec<String> = list.iter().map(|(v, _)| v.clone()).collect();
            let latest = versions.iter().max_by(|a, b| compare(a, b)).cloned();
            Ok(VersionList { versions, latest })
        }
        async fn manifest(&self, name: &str, version: &str) -> Result<Manifest, String> {
            let (_, deps) = self
                .0
                .get(name)
                .and_then(|l| l.iter().find(|(v, _)| v == version))
                .ok_or_else(|| format!("{name}#{version} not found"))?;
            Ok(Manifest {
                name: name.into(),
                version: version.into(),
                fhir_versions: vec!["4.0.1".into()],
                dependencies: deps.iter().cloned().collect(),
                title: Some(format!("{name} title")),
                description: None,
                canonical: None,
                url: None,
                kind: None,
                date: None,
                author: None,
                license: None,
            })
        }
    }

    fn request(name: &str, version: &str) -> PlanRequest {
        PlanRequest {
            name: name.into(),
            version: Some(version.into()),
            include_dependencies: true,
            fhir_release: "r4".into(),
            ..PlanRequest::default()
        }
    }

    fn twcore_world() -> MapSource {
        MapSource::new(&[
            (
                "tw.gov.mohw.twcore",
                "1.0.0",
                &[
                    ("hl7.fhir.r4.core", "4.0.1"),
                    ("hl7.terminology.r4", "7.0.0"),
                    ("hl7.fhir.uv.extensions.r4", "5.2.0"),
                    ("hl7.fhir.uv.ips", "1.1.0"),
                    ("hl7.fhir.uv.sdc", "3.0.0"),
                ],
            ),
            (
                "tw.gov.mohw.twcore",
                "0.3.2",
                &[("hl7.fhir.r4.core", "4.0.1")],
            ),
            (
                "hl7.terminology.r4",
                "7.0.0",
                &[
                    ("hl7.fhir.r4.core", "4.0.1"),
                    ("hl7.fhir.uv.extensions.r4", "5.2.0"),
                ],
            ),
            (
                "hl7.terminology.r4",
                "5.0.0",
                &[("hl7.fhir.r4.core", "4.0.1")],
            ),
            (
                "hl7.terminology.r4",
                "7.4.0",
                &[("hl7.fhir.r4.core", "4.0.1")],
            ),
            (
                "hl7.fhir.uv.extensions.r4",
                "5.2.0",
                &[("hl7.fhir.r4.core", "4.0.1")],
            ),
            (
                "hl7.fhir.uv.ips",
                "1.1.0",
                &[
                    ("hl7.terminology.r4", "5.0.0"),
                    ("hl7.fhir.r4.core", "4.0.1"),
                ],
            ),
            (
                "hl7.fhir.uv.sdc",
                "3.0.0",
                &[
                    ("hl7.fhir.r4.core", "4.0.1"),
                    ("hl7.fhir.r4.examples", "4.0.1"),
                ],
            ),
            (
                "tw.gov.mohw.nhi.pas",
                "1.2.7",
                &[
                    ("tw.gov.mohw.twcore", "0.3.2"),
                    ("hl7.terminology.r4", "7.4.0"),
                ],
            ),
            ("cycle.a", "1.0.0", &[("cycle.b", "1.0.0")]),
            ("cycle.b", "1.0.0", &[("cycle.a", "1.0.0")]),
        ])
    }

    #[tokio::test]
    async fn twcore_pulls_its_tree_once_with_core_embedded() {
        let source = twcore_world();
        let plan = resolve(&request("tw.gov.mohw.twcore", "1.0.0"), &[], &source)
            .await
            .unwrap();
        assert_eq!(plan.root, "tw.gov.mohw.twcore#1.0.0");
        // THO is required at 7.0.0 (twcore) and 5.0.0 (ips): installed once,
        // at the higher version, before everything that needs it.
        assert_eq!(
            plan.install_order,
            [
                "hl7.fhir.uv.extensions.r4#5.2.0",
                "hl7.terminology.r4#7.0.0",
                "hl7.fhir.uv.ips#1.1.0",
                "hl7.fhir.uv.sdc#3.0.0",
                "tw.gov.mohw.twcore#1.0.0"
            ]
        );
        let core = plan.node("hl7.fhir.r4.core").unwrap();
        assert_eq!(core.action, NodeAction::Embedded);
        assert_eq!(
            core.required.len(),
            5,
            "every package's core edge lands on one node"
        );
        assert!(matches!(
            plan.node("hl7.fhir.r4.examples").unwrap().action,
            NodeAction::Skipped { .. }
        ));
        let tho = plan.node("hl7.terminology.r4#7.0.0").unwrap();
        assert_eq!(tho.required.len(), 2, "{:?}", tho.required);
        assert!(plan.node("hl7.terminology.r4#5.0.0").is_none());
    }

    #[tokio::test]
    async fn exact_policy_installs_each_pinned_version() {
        let source = twcore_world();
        let mut req = request("tw.gov.mohw.twcore", "1.0.0");
        req.policy = DependencyPolicy::Exact;
        let plan = resolve(&req, &[], &source).await.unwrap();
        assert!(
            plan.install_order
                .contains(&"hl7.terminology.r4#7.0.0".to_string())
        );
        assert!(
            plan.install_order
                .contains(&"hl7.terminology.r4#5.0.0".to_string())
        );
    }

    #[tokio::test]
    async fn installed_newer_versions_satisfy_and_older_ones_are_upgraded() {
        let source = twcore_world();
        let installed = vec![
            ("tw.gov.mohw.twcore".to_string(), "1.0.0".to_string()),
            ("hl7.terminology.r4".to_string(), "7.0.0".to_string()),
        ];
        let plan = resolve(
            &request("tw.gov.mohw.nhi.pas", "1.2.7"),
            &installed,
            &source,
        )
        .await
        .unwrap();
        // twcore 0.3.2 is met by the installed 1.0.0.
        assert_eq!(
            plan.node("tw.gov.mohw.twcore#1.0.0").unwrap().action,
            NodeAction::Satisfied {
                by: "tw.gov.mohw.twcore#1.0.0".into()
            }
        );
        // THO 7.4.0 is newer than the installed 7.0.0: upgrade.
        assert_eq!(
            plan.node("hl7.terminology.r4#7.4.0").unwrap().action,
            NodeAction::Upgrade {
                from: vec!["7.0.0".into()]
            }
        );
        assert_eq!(
            plan.install_order,
            ["hl7.terminology.r4#7.4.0", "tw.gov.mohw.nhi.pas#1.2.7"]
        );
    }

    #[tokio::test]
    async fn root_upgrade_side_by_side_and_reinstall() {
        let source = twcore_world();
        let installed = vec![("tw.gov.mohw.twcore".to_string(), "0.3.2".to_string())];
        let mut req = request("tw.gov.mohw.twcore", "1.0.0");
        req.include_dependencies = false;
        let plan = resolve(&req, &installed, &source).await.unwrap();
        assert_eq!(
            plan.nodes[0].action,
            NodeAction::Upgrade {
                from: vec!["0.3.2".into()]
            }
        );
        assert_eq!(plan.install_order, ["tw.gov.mohw.twcore#1.0.0"]);
        assert!(
            plan.nodes[1..].iter().all(|n| !n.action.installs()),
            "dependencies off"
        );

        req.mode = InstallMode::SideBySide;
        let plan = resolve(&req, &installed, &source).await.unwrap();
        assert_eq!(plan.nodes[0].action, NodeAction::Install);

        let installed = vec![("tw.gov.mohw.twcore".to_string(), "1.0.0".to_string())];
        let plan = resolve(&req, &installed, &source).await.unwrap();
        assert_eq!(plan.nodes[0].action, NodeAction::Reinstall);
    }

    #[tokio::test]
    async fn skipped_and_missing_dependencies_are_reported_not_fatal() {
        let source =
            MapSource::new(&[("a", "1.0.0", &[("not.published", "1.0.0"), ("b", "2.0.0")])]);
        let mut req = request("a", "1.0.0");
        req.skip.insert("b".into());
        let plan = resolve(&req, &[], &source).await.unwrap();
        assert!(matches!(
            plan.node("not.published#1.0.0").unwrap().action,
            NodeAction::Missing { .. }
        ));
        assert!(matches!(
            plan.node("b").unwrap().action,
            NodeAction::Skipped { .. }
        ));
        assert_eq!(plan.install_order, ["a#1.0.0"]);
        assert_eq!(plan.warnings.len(), 1);

        // The root itself must exist.
        assert!(
            resolve(&request("nope", "1.0.0"), &[], &source)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cycles_terminate() {
        let source = twcore_world();
        let plan = resolve(&request("cycle.a", "1.0.0"), &[], &source)
            .await
            .unwrap();
        assert_eq!(plan.install_order, ["cycle.b#1.0.0", "cycle.a#1.0.0"]);
    }

    #[tokio::test]
    async fn latest_is_resolved_through_the_source() {
        let source = twcore_world();
        let mut req = request("hl7.terminology.r4", "x");
        req.version = None;
        let plan = resolve(&req, &[], &source).await.unwrap();
        assert_eq!(plan.root, "hl7.terminology.r4#7.4.0");
        assert!(matches!(
            special("hl7.fhir.r5.core", "r4"),
            Some(NodeAction::Skipped { .. })
        ));
    }
}
