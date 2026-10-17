# FHIR packages (Implementation Guides): operator guide

HFS installs FHIR npm packages (IGs such as `tw.gov.mohw.twcore`,
`hl7.fhir.us.core`, `hl7.fhir.uv.ips`) with their dependencies, keeps a
registry of what is installed and which resources each package owns, and
removes a package again without touching anything else. This page covers
how to turn it on, what an install does, and how to run it in a hospital
without internet access.

## Turning it on

| Variable | Default | |
|---|---|---|
| `HFS_ADMIN_UI` | `false` | `true` enables the `/admin/packages` API and the console page **IG Packages** (`/ui/packages`, in the navigation and the phone drawer). |
| `HFS_ADMIN_PASSWORD` / `HFS_ADMIN_PASSWORD_FILE` | (none) | Administrator password (or a file holding it). Required with `HFS_ADMIN_UI=true` unless SMART/OIDC auth is on; see [Administrator sign-in](admin-sign-in.md). |
| `HFS_PACKAGE_REGISTRY` | `on` | `on`: packages.fhir.org, then packages2.fhir.org. `off`: no network access at all. A comma-separated list of URLs: npm-compatible mirrors used instead of the public registries. |
| `HFS_PACKAGES` | (none) | `name#version,...` installed at startup when missing (works with `HFS_ADMIN_UI=false`). |
| `HFS_PACKAGE_CACHE_DIR` | next to the SQLite database (`/data/packages` for `HFS_DATABASE_URL=/data/fhir.db`), else `<HFS_DATA_DIR or ./data>/packages` | The package cache. |
| `HFS_PACKAGE_MARKETPLACE` | FHIR/ig-registry `fhir-ig-list.json` on GitHub | The marketplace index, or `off`. Not fetched when the registry is `off`. |
| `HFS_PACKAGE_MAX_SIZE` | 256 MiB | Largest `.tgz` (upload or download). |
| `HFS_PACKAGE_MAX_UNPACKED` | 1 GiB | Largest unpacked package. |
| `HFS_PACKAGE_MAX_ENTRY` | 64 MiB | Largest single file inside a package. |
| `HFS_TERMINOLOGY_SERVER` | (none) | Where CodeSystem/ValueSet/ConceptMap go (`POST /import`). Without it terminology is stored in HFS. |
| `HFS_TERMINOLOGY_SERVER_TOKEN` / `_FILE` | (none) | Bearer token for those terminology writes; HTS's administrator password when HTS requires auth for writes (`HTS_REQUIRE_AUTH_FOR_WRITES`). |

Installing and removing packages are administrative actions. They sit
behind `HFS_ADMIN_UI` and the administrator sign-in (`HFS_ADMIN_PASSWORD`;
browsers sign in at `/ui/admin/login`, scripts send `curl -u admin:…` — see
[Administrator sign-in](admin-sign-in.md)); with `HFS_ADMIN_UI=true` and no
password the API answers `503`. When SMART/OIDC authentication is on, the
API instead requires a system-scope token (the same tier as
`/admin/tenants`). Leave `HFS_ADMIN_UI` off on servers where nobody should
change the installed IGs, and use `HFS_PACKAGES` instead.

## What an install does

1. **Plan (dry run).** The package's `package.json` dependencies are
   resolved recursively. `hl7.fhir.r4.core` is never imported: the server
   embeds the core definitions (importing r4.core StructureDefinitions breaks
   the validator). `hl7.fhir.r4.examples`/`expansions` are skipped. The plan
   shows the tree, what goes where per resource type, and conflicts with
   what is already stored.
2. **Dependency versions.** By default an installed (or planned) version
   *at or above* the one a package asks for satisfies it, and when several
   packages ask for different versions of the same dependency the highest is
   installed once. Without this, the seven Taiwan IGs alone would install
   seven versions of `hl7.terminology.r4`. The *exact* policy installs every
   pinned version side by side instead. Single dependencies can be skipped in
   the plan; VSAC and PHIN VADS (pulled by US Core) are large: 14 k and 2 k
   ValueSets.
3. **Install (background job, cancellable).**
   - StructureDefinition, SearchParameter, CapabilityStatement,
     OperationDefinition, ImplementationGuide, Questionnaire, NamingSystem,
     StructureMap and every other conformance type go into HFS.
     StructureDefinitions are registered with the validator as they are
     written and replayed from storage at every start, so
     `POST /Patient/$validate` against an installed profile works right away
     and after a restart.
   - CodeSystem, ValueSet and ConceptMap go to the terminology server
     (option: HFS only, or both). NamingSystem stays in HFS (HTS does not
     store it).
   - Examples (`package/example/`, or resources the IG marks as examples)
     are skipped unless *Include examples* is ticked.
   - New SearchParameters apply to data written afterwards; the result page
     offers `$reindex` for existing data.
4. **Upgrade.** Installing another version of an installed package replaces
   it by default: the new version is written, then what only the old
   version owned is removed and dependents are pointed at the new version.
   *Side by side* keeps both; ids already taken by the other version get a
   `-<version>` suffix.

### Ownership and removal

Every written resource is recorded against its package (tables
`hfs_packages` and `hfs_package_resources` in the SQLite or PostgreSQL
database, created on first use; a `registry.json` in the cache directory on
other backends). Uninstall:

- is refused (`409`) while another installed package depends on the package;
- deletes only resources no other package also owns;
- never deletes a resource that existed before the install, unless it was
  the same artifact (same canonical url and version, e.g. loaded earlier by
  a script), which the package adopts;
- never deletes terminology that was already on the terminology server;
- unregisters deleted profiles, and re-registers another package's profile
  that has the same canonical;
- writes an AuditEvent, like every install.

Two IGs that use the same id for different resources (TW Core and EMR both
have `SearchParameter/Condition-code`) do not overwrite each other: the
second is stored as `<id>-<8 hex of its canonical's SHA-1>`.

## Offline hospitals

Hospitals usually have no outbound internet. Three ways to get packages in:

1. **Upload.** Set `HFS_PACKAGE_REGISTRY=off`. On **IG Packages → Upload
   .tgz**, drop the package. The archive is checked (no absolute or `..`
   paths, size limits, a valid `package.json`) and kept in the cache; the
   plan then lists any dependency that is neither installed nor cached as
   *not found*. Upload those too (dependencies first is not required: the
   plan is recomputed), or skip them.
2. **Cache directory.** Copy `name#version.tgz` files (the exact archive
   from the registry, e.g. `https://packages.fhir.org/<name>/<version>`) into
   `HFS_PACKAGE_CACHE_DIR`. Cached packages install without the network,
   through the console (*Install by package name*) or `HFS_PACKAGES`.
   The marketplace index can be provided the same way as
   `fhir-ig-list.json` in that directory.
3. **Mirror.** Point `HFS_PACKAGE_REGISTRY` at an internal npm-compatible
   registry (Simplifier/Firely, Verdaccio, a static mirror). It must answer
   `GET <base>/<name>` (npm document with `versions.<v>.dist.tarball` and
   `shasum`) and `GET <base>/<name>/<version>` (the archive); `/catalog`
   is used for search when present. Downloads are checked against `shasum`.

Example: a Taiwanese hospital server that always has TW Core and IPS,
installed from a pre-filled cache volume:

```yaml
environment:
  HFS_DATABASE_URL: /data/fhir.db
  HFS_PACKAGE_REGISTRY: "off"
  HFS_PACKAGES: tw.gov.mohw.twcore#1.0.0,hl7.fhir.uv.ips#1.1.0
  HFS_TERMINOLOGY_SERVER: http://hts:8090
volumes:
  - hfs-data:/data          # packages cached in /data/packages
```

## API

All under the server root (behind a proxy, under the FHIR base path):

| | |
|---|---|
| `GET /admin/packages` | installed packages |
| `POST /admin/packages` | JSON `{"package": "name#version", "dryRun": true, "options": {...}}` → plan or install job; a `.tgz` body (`Content-Type: application/gzip`) → upload + plan (`?dryRun=false` installs) |
| `GET /admin/packages/{name}/{version}` | one package with its resources by type |
| `DELETE /admin/packages/{name}/{version}` | uninstall job |
| `GET /admin/packages/jobs`, `GET /admin/packages/jobs/{id}`, `DELETE /admin/packages/jobs/{id}` | jobs, progress and results; cancel |
| `GET /admin/packages/registry/search?q=&fhirversion=R4` | registry search (Taiwan first) |
| `GET /admin/packages/registry/marketplace?q=&country=&category=&authority=` | marketplace index |
| `GET /admin/packages/registry/versions/{name}` | published versions |
| `GET /admin/packages/cache`, `GET /admin/packages/config` | cache contents, effective configuration |

Options: `includeExamples` (false), `includeDependencies` (true), `mode`
(`upgrade` \| `side-by-side`), `dependencyPolicy` (`newer` \| `exact`),
`skip` (dependency names), `terminology` (`auto` \| `hfs` \| `both`).
Job-creating calls take `?wait=<seconds>` (up to 60) to return a finished
job in one call.

```sh
curl -X POST 'http://localhost:8080/admin/packages?wait=60' \
  -H 'Content-Type: application/json' \
  -d '{"package":"tw.gov.mohw.twcore#1.0.0","dryRun":true}'
curl -X POST 'http://localhost:8080/admin/packages' \
  -H 'Content-Type: application/gzip' --data-binary @package.tgz
```

## Limits and notes

- Jobs live in memory: a restart loses the progress view, not the result.
  A package interrupted mid-install is listed as `installing`/`cancelled`;
  remove or reinstall it.
- One install or uninstall runs at a time; plans run concurrently.
- Packages are installed into the tenant of the request (`X-Tenant-ID`,
  default `HFS_DEFAULT_TENANT`); the terminology server is shared.
- The profile registry resolves a canonical to one definition: with two
  versions installed side by side, the last one written wins for
  `meta.profile` claims without a version.
