# HealthyMind FHIR Server Manager (based on Helios FHIR Server) — UI message catalog
# Locale: English (en) — SOURCE LOCALE. Every key defined here is the
# canonical set; other locales are expected to provide the same keys.
#
# Syntax: Project Fluent (https://projectfluent.org/). Terms (prefixed with
# `-`) are reusable snippets; messages (bare identifiers) are what the UI
# looks up. Placeables `{ $var }` are interpolated by the caller. Do NOT put
# markup or logic here — translations are data, the template renders them.

## Brand / shared terms

-app-name = HealthyMind FHIR Server Manager
-org-name = HealthyMind Tech

## Page chrome

app-title = { -app-name }
app-short-name = HealthyMind FHIR
app-product-line = FHIR Server Manager
app-tagline = Many FHIR endpoints, one manager — based on Helios FHIR Server

nav-dashboard = Dashboard
nav-terminology = Terminology
nav-terminology-new-window = Terminology (opens in a new tab)
nav-resources = Resources
nav-settings = Settings
nav-signout = Sign Out

## Language switcher

language-label = Language
language-en = English
language-es = Spanish
language-de = German
language-zh-hant = Traditional Chinese
user-menu-label = Account menu
user-anonymous = Anonymous user
user-local-hint = Authentication is disabled
user-logout = Sign out

## Home / landing page

home-lede = Server-rendered, HTMX-first UI. This panel is refreshed as an HTML fragment.

## Status panel

status-last-checked = Last checked: { $timestamp }

## Dashboard / health

dashboard-heading = Server Dashboard
health-status-ok = All systems operational
health-status-degraded = Some systems are degraded
health-uptime = Uptime: { $duration }

# Pluralized count — every locale must supply the plural categories its
# grammar requires (CLDR rules; Fluent selects the branch automatically).
resource-count = { $count ->
    [one] { $count } resource
   *[other] { $count } resources
}

## Terminology browsing

terminology-heading = Terminology Server
terminology-lede = Connect HFS to a FHIR terminology server.
terminology-configured-heading = Terminology Server Configured
terminology-configured-body = HFS_TERMINOLOGY_SERVER points to a valid server URL.
terminology-configured-open = Open Terminology Server
terminology-invalid-heading = HFS_TERMINOLOGY_SERVER is invalid.
terminology-invalid-body = Use an absolute HTTP or HTTPS URL with a host. Paths and a trailing slash are allowed. Do not include credentials, a query string, or a fragment.
terminology-invalid-note = Update the environment variable, then restart HFS.
terminology-setup-heading = Connect a Terminology Server
terminology-setup-body = Set HFS_TERMINOLOGY_SERVER to the base URL of the FHIR terminology server that HFS should use.
terminology-setup-note = Set the variable in the environment that starts HFS, then restart the server.
terminology-search-label = Search CodeSystems and ValueSets
terminology-search-placeholder = e.g. 73211009, "diabetes", http://snomed.info/sct
terminology-display-language = Display language
terminology-no-results = No matching concepts found.

## Common actions

action-search = Search
action-save = Save
action-cancel = Cancel
action-back = Back
action-retry = Retry

## Errors (mirrors OperationOutcome text; see crates/ui/docs/multi-language.md §5)

error-not-found = The requested resource was not found.
error-unauthorized = You are not authorized to perform this action.
error-generic = Something went wrong. Try again.

## Generic "not found" page (#835): a full-page 404 inside the shell, distinct
## from error-not-found above (OperationOutcome text for API responses).

not-found-title = Not found
not-found-text = This page doesn't exist, or isn't available to you.

## Dashboard shell (Figma "Dashboard V1.1")

nav-section-work = Work
nav-section-batch-data = Batch & Data
nav-section-sql-on-fhir = SQL on FHIR
nav-section-server = Server
nav-section-tools = Tools

nav-home = Home
nav-search = Search
nav-resource-editor = Resource Editor
nav-history-versions = History & Versions
nav-compartments = Compartments
nav-batch-transaction = Batch / Transaction
nav-import = Import
nav-export = Export
nav-sql-view-definitions = View Definitions
nav-sql-queries = SQL Queries
nav-sql-views = SQL Views
nav-sql-export = SQL Export
nav-capability-conformance = Capability & Conformance
nav-search-parameters = Search Parameters
nav-subscriptions = Subscriptions
nav-tenants = Tenants

## Tenant maintenance (/ui/tenants)

tenants-title = Tenant Maintenance
tenants-lede = Provision, inspect, and delete the tenants this server isolates data between.
tenants-unavailable = The tenant registry is not available on this storage backend.
tenants-stat-total = Total tenants
tenants-stat-total-sub = { $count ->
    [one] { $count } registered
   *[other] { $count } registered
}
tenants-stat-resources = Resources stored
tenants-stat-resources-sub = across all tenants
tenants-search-placeholder = Search by name or tenant id…
tenants-add = Add Tenant
tenants-add-title = Add a Tenant
tenants-field-id = Tenant id
tenants-field-id-hint = Used in the API (X-Tenant-ID header, URL prefix, JWT claim).
tenants-field-name = Display name (optional)
tenants-field-name-hint = A human-friendly label; not used for routing.
tenants-add-submit = Provision Tenant
tenants-col-tenant = Tenant
tenants-col-resources = Resources
tenants-col-created = Created
tenants-col-actions = Actions
tenants-empty = No tenants match.
tenants-unregistered = unregistered
tenants-delete = Delete Tenant
tenants-delete-confirm = Deregister tenant "{ $id }"? Its stored data is kept unless purged via the API.
tenants-row-provisioning = Provisioning… this may take a moment.
tenants-row-failed = Could not provision the tenant.
tenants-dismiss = Dismiss
# Submit-time errors on the Add Tenant form (#317 mirrors the API's id rules).
tenants-error-id-empty = Tenant id must not be empty.
tenants-error-id-too-long = Tenant id exceeds { $max } characters.
tenants-error-id-reserved = That id is reserved for internal shared resources.
tenants-error-id-invalid-character = Tenant id may contain only letters, digits, '-', '_', '.', and '/'.
tenants-error-id-empty-segment = Tenant id must not begin or end with '/' or contain an empty segment.
tenants-error-exists = Tenant '{ $id }' already exists.
tenants-error-provisioning = Tenant '{ $id }' is already being provisioned.
tenants-error-still-provisioning = Tenant '{ $id }' is still being provisioned.
tenants-error-not-purged = Tenant '{ $id }' was deregistered but its data was NOT purged: { $detail }
tenants-error-list = Failed to list tenants: { $detail }
tenants-error-count = Failed to count resources: { $detail }
tenants-field-name-placeholder = Acme Health
tenants-stat-total-sub-partial = { $count ->
    [one] { $count } registered · discovery incomplete
   *[other] { $count } registered · discovery incomplete
}
tenants-stat-resources-sub-stale = across all tenants · may be out of date
tenants-stat-pending = Counting…
tenants-stat-unavailable = Not available right now
tenants-stat-unsupported = Not available on this storage
tenants-counts-pending = Counting resources across tenants…
tenants-counts-refreshing = Updating resource counts; showing counts from { $time }.
tenants-counts-ready = Resources counted at { $time }.
tenants-counts-partial = Resources counted at { $time }, but not every tenant could be checked.
tenants-counts-stale = Counts from { $time } may be out of date: the latest count failed.
tenants-counts-unavailable = Resource counts are not available right now.
tenants-counts-unsupported = This storage cannot count resources per tenant.
tenants-counts-presence = This storage reports which tenants hold data, not how many resources.
tenants-counts-approx-pointers = Counts are approximate: storage pointers, deleted resources included.
tenants-counts-approx-index = Counts come from the search index and may lag recent writes.
tenants-counts-refresh = Refresh
tenants-count-pending = Counting…
tenants-count-unknown = Not known
tenants-count-unsupported = Not counted on this storage
tenants-count-unavailable = Count unavailable
tenants-count-present = has data
tenants-count-present-note = Holds data; this storage does not count it
tenants-count-stale = may be out of date
tenants-count-approx = approximate, not an exact count of live resources

tenant-heading = Tenants
tenant-all = All tenants
tenant-search-placeholder = Search tenants

theme-toggle-system = Theme: follow system
theme-toggle-light = Theme: light
theme-toggle-dark = Theme: dark

fhir-version = FHIR { $version }
fhir-version-heading = FHIR Version

card-resource-types = Resource Types
card-resource-types-sub = used for { $version }
card-stored-resources = Stored Resources
card-stored-resources-sub = across active tenant
card-export-jobs = Export Jobs
card-export-jobs-sub = running ({ $queued } queued)
card-import-jobs = Import Jobs
card-import-jobs-sub = active
card-jobs-unavailable = unavailable
card-uptime = Uptime
card-uptime-sub = since process start

chart-title = FHIR Resources over Time
chart-window = Chart time window
chart-pick-heading = Charted Resource Types
chart-pick-all = View all resource types
chart-pick-filter = Filter types
chart-empty = Nothing to chart yet — stored resources appear here as they are created.
# Only for a build with no metrics provider at all. A provider that was merely
# slow gets chart-pending-note instead — the two must never be confused (#956).
chart-sample-note = Sample data: this build has no live metrics provider, so the chart below is a placeholder, not measurements.
chart-pending-note = Still gathering the live figures for this window. Nothing is charted until they arrive — no placeholder numbers are shown.
chart-pending-empty = Waiting for the live figures…
chart-pending-retry = Retry now
# #1078: measured, but counted from in-memory write counters rather than an
# exact storage read. A label, not a warning.
chart-approximate-note = Approximate: counted from recent writes and still being reconciled with storage.
chart-approximate-history-note = Approximate: counted from recent writes and still being reconciled with storage. A dashed line shows only the writes recorded since the counters were read from storage; its earlier history is still loading.
chart-legend-history-loading = history loading
# #1078: the storage backend cannot count resources at all (e.g. an S3
# primary). Shown in the chart area; the notice line below explains it.
chart-counts-unsupported = Resource counts are not available for this storage backend.
chart-counts-unsupported-note = This storage backend cannot count stored resources, so no totals or chart are shown. This does not mean the tenant is empty.
# #1078: when the figures on the page were read. $time is a UTC time,
# prefixed with the date when it is not today (e.g. "14:02:31 UTC").
chart-as-of = As of { $time }.
# #1065: a search-index rebuild ($reindex) is running for this tenant. Stored
# resources stay readable by id, but searches can miss them until it finishes.
# $percent is a whole number; $processed and $total are resource counts.
search-index-rebuilding = Search index rebuilding — { $percent }% ({ $processed } of { $total } resources). Searches may miss stored resources until it finishes.
# The same, before the rebuild has counted the resources it will process.
search-index-rebuilding-counting = Search index rebuilding. Searches may miss stored resources until it finishes.
# #1125: the tenant's most recent search-index rebuild ended with resources
# left unindexed. $errors is a resource count; $job is the rebuild job id.
search-index-rebuild-failed = { $errors ->
    [one] The last search index rebuild left one resource unindexed. Searches miss it until a rebuild succeeds; GET $reindex-status/{ $job } says which one.
   *[other] The last search index rebuild left { $count } resources unindexed. Searches miss them until a rebuild succeeds; GET $reindex-status/{ $job } lists which ones.
  }
# The same, when the rebuild failed as a whole before naming any resource.
search-index-rebuild-failed-job = The last search index rebuild failed before it finished. Searches may miss stored resources until a rebuild succeeds; GET $reindex-status/{ $job } says why.
chart-table-toggle = View as Table
chart-table-when = Time
chart-focus-series = Focus this series
chart-unfocus-series = Show all series

## Footer

footer-copyright = © { $year } { -org-name }

## History & Versions (#236)

history-heading = History & Versions
history-lede = Compare two versions of a resource. Storage is fully versioned; this reads it through the ordinary _history and vread API.
history-type-label = Resource type
history-id-label = Resource id
history-id-placeholder = resource id
history-load = Load
history-tabs-label = History scope
history-tab-instance = Instance
history-tab-type = Type Feed
history-tab-system = System Feed
history-versions-label = Versions
history-current = current
history-op-create = created
history-op-update = updated
history-op-patch = patched
history-op-delete = deleted
history-op-version = version
history-from = From
history-to = To
history-show-metadata = Show metadata changes
history-empty = Load a resource, then pick two versions to compare.
history-load-error = Could not load that resource's history.
history-not-found = No history for that resource — check the type and id.
history-feed-resource = Resource
history-feed-version = Version
history-feed-interaction = Interaction
history-feed-when = When
history-feed-more = Load more
history-feed-empty = No history entries yet.
history-feed-error = Could not load the history feed.
history-diff-heading = { $from }
history-metadata-hidden = { $count ->
    [one] { $count } metadata change hidden
   *[other] { $count } metadata changes hidden
}
history-textual = Show full text diff
history-only-metadata = Only metadata changed between these versions.
history-identical = These two versions are identical.
history-deleted = { $version } is a deletion — there is nothing to diff against.
history-parse-error = Those versions could not be read as JSON.
## Shared search builder (#234)

queries-run = Run
queries-delete = Delete

## SearchParameter viewer (#238)

sp-heading = Search Parameters
sp-lede = Browse the parameters this server resolves searches against, filtered by base resource type. Stored parameters can be created, edited, and deleted; the registry picks changes up per tenant.
sp-version-label = FHIR version
sp-degraded = Search parameters could not be loaded from this server right now — the self-call to /SearchParameter failed (with authentication enabled this usually means the outbound service token is missing or invalid). The page retries on the next request.
sp-degraded-unsupported = Search parameters cannot be listed on this storage backend — the self-call to /SearchParameter answered 501 Not Implemented because the backend has no search for this type.
sp-empty = No search parameters are stored on this server for this FHIR version, so there is nothing to list.
sp-rail-label = Resource filter
sp-rail-search = Filter types
sp-rail-recent = Recently used
sp-rail-types = Resource types
sp-rail-all = All Types
sp-facet-type = Type
sp-facet-type-label = Filter by parameter type
sp-facet-source = Source
sp-facet-source-label = Filter by source
sp-source-embedded = embedded
sp-source-stored = stored
sp-source-config = config
sp-chip-conflict = conflict
sp-chip-duplicate = same definition
sp-chip-overrides = overrides spec
sp-chip-shadowed = shadowed
sp-col-code = Code
sp-col-type = Type
sp-col-base = Base
sp-col-expression = Expression
sp-col-source = Source
sp-total = { $count } parameters
sp-pagination-label = Pages
sp-page-prev = Previous
sp-page-next = Next
sp-detail-label = Parameter detail
sp-detail-close = Close detail
sp-detail-readonly = Spec parameter (compiled in from the data file) — read-only.
sp-field-url = Canonical URL
sp-field-name = Name
sp-field-status = Status
sp-field-base = Base resource types
sp-field-expression = FHIRPath expression
sp-field-description = Description
sp-field-target = Target types
sp-field-components = Components
sp-status-hint = The loader promotes the spec's draft status to active on load.
sp-note-conflict = Duplicate (base, code) within the same source as { $url } — the registry rejects this collision (DuplicateCode).
sp-note-duplicate = Same definition as { $url }: identical type and expression, so searches behave the same whichever one the registry keeps. Nothing to fix.
sp-note-overrides = Overrides { $url } on (base, code): a Stored definition outranks the spec parameter, so this one resolves searches. The registry logs a WARN naming both URLs.
sp-note-shadowed = Shadowed by { $url } on (base, code): a higher-precedence source resolves searches for this slot.
sp-note-empty-expression = Empty expression: the extractor indexes zero rows, so every search on this parameter silently returns empty.
sp-note-no-target = Reference parameter with no target types: chained search cannot resolve the referenced type.
sp-note-choice-type = Choice-type expression: the extractor rewrites ofType(T) / as T to the concrete element (for example valueQuantity) before evaluating against raw stored JSON.
sp-new = New Search Parameter
sp-edit = Edit
sp-delete = Delete
sp-delete-confirm = Delete this stored search parameter? Searches that use it stop matching once the registry refreshes.
cmp-new = New Compartment Definition
cmp-edit = Edit
cmp-delete = Delete
cmp-delete-confirm = Delete this compartment definition? Its compartment routes stop resolving.
crud-delete-failed = Could not delete this item.

## Compartment viewer & tester (#237)

cmp-heading = Compartments
cmp-lede = The compartment definitions this server routes /{"{"}compartment{"}"}/{"{"}id{"}"}/{"{"}type{"}"} requests with, and a tester that answers: is this type in this compartment, via which parameters, and what search does the server run?
cmp-rail-label = Compartment definitions
cmp-rail-heading = Compartments
cmp-degraded = Compartment definitions could not be loaded from this server right now — the self-call to /CompartmentDefinition failed (with authentication enabled this usually means the outbound service token is missing or invalid). The page retries on the next request.
cmp-degraded-unsupported = Compartment definitions cannot be listed on this storage backend — the self-call to /CompartmentDefinition answered 501 Not Implemented because the backend has no search for this type.
cmp-empty = No compartment definitions are stored on this server for this FHIR version, so there is nothing to list.
cmp-rail-note = Definitions are stored resources, seeded from the FHIR spec at startup. Edits and deletions here are tenant-scoped.
cmp-tabs-label = Compartment sections
cmp-tab-definition = Definition
cmp-tab-members = Members
cmp-tab-tester = Tester
cmp-field-code = Code
cmp-field-status = Status
cmp-field-url = Canonical URL
cmp-field-version = Version
cmp-field-publisher = Publisher
cmp-field-description = Description
cmp-field-search = search
cmp-field-experimental = experimental
cmp-search-why = Off would mean no compartment route resolves for this compartment.
cmp-on = on
cmp-off = off
cmp-yes = yes
cmp-no = no
cmp-readonly-note = Read-only: these values come from the spec definitions compiled into the server.
cmp-filter-members = Members
cmp-filter-all = All Types
cmp-filter-excluded = Excluded
cmp-member = member
cmp-excluded = excluded
cmp-tester-id = Id
cmp-tester-target = Target type (or *)
cmp-tester-run = Test
cmp-result-member = ✓ member — via { $params }
cmp-result-flat = // equivalent flat search
cmp-result-member-note = The server resolves the compartment route to this search over the type's reference parameters.
cmp-result-self = ✓ member — the compartment resource itself ({"{"}def{"}"})
cmp-result-self-note = The compartment instance is trivially in its own compartment; the route reads the resource directly.
cmp-result-notmember = ✕ { $type } is not a member of this compartment
cmp-result-notmember-note = The server returns 404 with an OperationOutcome for types that are not compartment members.
cmp-result-fanout = Fans out to { $count } member types
cmp-result-fanout-note = Excluded types are skipped, not failed — the fan-out drops non-member types rather than erroring.
queries-builder-heading = Search Builder
queries-url-label = FHIR search URL
queries-url-placeholder = GET /Patient?name=smith&birthdate=ge1980-01-01
queries-recent = Recent
queries-recent-heading = Recent Searches
queries-recent-empty = No recent searches yet — Run one to record it here.
queries-invalid-url = Enter a search like GET /Patient?name=smith — the resource type comes from the path.
queries-invalid-fhir-escape = This query contains an invalid FHIR escape. Correct the escaped value before editing it visually.

queries-conditions = Conditions
queries-add-condition = Add condition
queries-includes = Includes
queries-result-controls = Result controls
queries-remove = Remove
queries-match-is = is
queries-or = + or
plain-pill = In plain English
plain-find = Find {"{type}"} records
plain-read = Open {"{type}"} {"{id}"}
plain-clause = {"{path}"} {"{verb}"} {"{value}"}
plain-clause-no-value = {"{path}"} {"{verb}"}
plain-and = and
plain-or = or
plain-arrow = ’s
plain-has = that have a related {"{type}"} whose {"{param}"} {"{verb}"} {"{value}"}
plain-has-no-value = that have a related {"{type}"} whose {"{param}"} {"{verb}"}
plain-include = Also returning the {"{param}"} of each {"{type}"}{"{target}"}
plain-revinclude = Plus every {"{type}"} whose {"{param}"} points here
plain-iterate = (repeatedly)
plain-count = Showing {"{n}"} per page
plain-unknown-param = "{"{param}"}" is not a search parameter for {"{type}"}
plain-sort = Sorted by {"{sort}"}
plain-verb-is = is
plain-verb-contains = contains
plain-verb-exact = is exactly
plain-verb-missing = is present/absent
plain-verb-missing-true = is absent
plain-verb-missing-false = is present
plain-verb-not = is not
plain-verb-text = matches the text
plain-verb-in = is in the value set
plain-verb-not-in = is not in the value set
plain-verb-identifier = has the identifier
plain-verb-of-type = has an identifier of type
plain-verb-ge = is on or after
plain-verb-le = is on or before
plain-verb-gt = is after
plain-verb-lt = is before
plain-verb-ne = is not
plain-verb-eq = is
plain-verb-sa = starts after
plain-verb-eb = ends before
plain-verb-ap = is approximately
queries-related-heading = Include Related Data
queries-related-sub = Adds connected resources to the results.
queries-related-add-include = Include a resource that points to
queries-related-add-revinclude = Include resources that point here
queries-iterate = Iterate
queries-sort-label = Sort
queries-sort-default = Default
queries-sort-recent = Most recent
queries-sort-oldest = Oldest
queries-sort-id = ID
queries-modify-heading = Modifiers
queries-mod-exact = whole value incl. case & accents
queries-mod-contains = match anywhere in the text
queries-mod-missing = field is present / absent
queries-mod-text = advanced text handling
queries-mod-not = none of the values match
queries-mod-above = this or an ancestor
queries-mod-below = this or a descendant
queries-mod-in = member of the value set
queries-mod-not-in = not a member of the value set
queries-mod-identifier = match the reference by identifier
queries-mod-of-type = match identifier type, system and value
queries-chain-into = Filter by a property of the referenced resource
queries-chain-any-target = any
queries-has-pill = has a related
queries-has-type-placeholder = resource type
queries-has-via = linked via
queries-has-where = where its
queries-add-has = ⧉ Filter a resource that links here
queries-param-placeholder = parameter
queries-param-none = No matching parameters
queries-param-unknown = Not a search parameter for {"{type}"}. Pick one from the list.
queries-value-placeholder = value
queries-results = Results
queries-results-total = { $count } results
queries-results-total-partial = { $count }+ results
queries-results-included = { $count } included
queries-results-empty = No results.
queries-searching = Searching…
queries-search-elapsed = { $seconds } seconds elapsed
queries-search-cancel = Cancel
queries-search-slow = This search is taking longer than expected. You can keep waiting or cancel it.
queries-search-keep-waiting = Keep waiting
queries-results-previous = Previous results
queries-col-updated = Updated
queries-copy-id = Copy id
queries-copied = Copied
queries-prev = Previous
queries-next = Next
queries-results-fetch-error = Could not load results from { $origin }. Check HFS_BASE_URL and try again.

queries-rail-heading = Resource Types
queries-rail-filter = Filter types

## Search — natural language & visual builder (#255)

search-heading = Search
search-lede = Describe what you're looking for, or build the query by hand. Either way you get a FHIR search query you can read, correct, and run.
search-query-tag = QUERY
search-copy = Copy the Query

search-mode-label = How to write the query
search-mode-nl = Natural language
search-mode-builder = Visual builder

search-nl-label = Describe the search
search-nl-placeholder = Describe what you're looking for — e.g. patients named Smith born after 1980
search-nl-hint = Your text and this server's search parameters go to the language model. Patient data never does. The query it writes is shown below for you to check and run.
search-nl-working = Translating…
search-nl-caveats = Worth knowing:
search-nl-unsupported = That isn't a search this server can run. Try describing the records you want to find.

search-nl-example-1 = Female patients over 65 with a diabetes diagnosis
search-nl-example-2 = Observations from the last 30 days, most recent first
search-nl-example-3 = Encounters at Boston General still in progress

search-setup-heading = Natural-language search is available
search-setup-body = Turn plain-language descriptions into FHIR search queries. It needs an API key for a language model — the server reads it from the environment, and it never reaches this page. Until one is set, use the visual builder below.
search-setup-key-placeholder = your API key
search-setup-disable = To remove the feature entirely — endpoint, page, and this notice — set HFS_NL_SEARCH_ENABLED=false.
search-setup-docs = Read the how-to

## Resource editor (#264)

editor-heading = Resource Editor
editor-lede = Edit a resource against its schema: add any element the schema allows, at any depth — including extensions, on any node that accepts one.
editor-title = Edit Resource
editor-view-label = How to edit
editor-view-form = Guided form
editor-view-json = JSON
editor-save = Save Changes
editor-delete = Delete
editor-remove = Remove This Node
editor-saved = Saved.
# Header notice while creating a document that carries an id (#1751).
editor-save-target = Will be saved as { $target }
editor-id-exists = { $target } already exists. Saving will add a new version of it.
editor-id-exists-confirm = Save new version
# Shared unsaved-changes tracker (#1240): the pill next to a Save button and
# the confirm shown for in-page closes (a modal, an addbox disclosure).
unsaved-changes = Unsaved changes
unsaved-discard-confirm = You have unsaved changes. Discard them and close?
unsaved-leave-confirm = You have unsaved changes. Discard them and leave this page?
unsaved-leave-action = Discard and leave
# The shared in-page confirmation's confirm button (#1667); its cancel
# button reuses action-cancel.
confirm-dialog-ok = Confirm
editor-load-error = Could not load that resource.
editor-load-not-found = No { $type } with id { $id } exists on this server — check the id, or go back to the list.
editor-confirm-delete = Delete this resource? This cannot be undone.
editor-invalid-json = That is not valid JSON, so it cannot be edited as a form. Your text is untouched.
editor-source-hint = Edit here or in the guided form: both always show the same document.
editor-format = Format
editor-format-title = Format the document (Shift+Alt+F)
editor-format-invalid = Fix the JSON syntax errors before formatting.

editor-add = Add Element
editor-add-close = Close
editor-add-added = added
editor-add-undo = Undo
editor-collection-add = Add
editor-add-undo-pending = Undo is unavailable while the form is updating.
editor-add-undo-unavailable = Undo is no longer available after the document changed.
editor-update-failed = The form update failed. Retry the changed field before adding or removing an element.
editor-add-elements = Elements
editor-add-extensions = Extensions
editor-must-support-badge = MS
editor-binding-hint = Bound to a value set — codes come from it; strength shown
editor-legend-live = Checked as you type: structure, cardinality, required bindings
editor-legend-save = Checked on save: constraints and terminology
# ViewDefinition's own single-line legend (#843): Save stays permissive
# there, so promising a "checked on save" pass would be misleading — this
# names everything the guided form's per-keystroke pass actually covers,
# folding in the SQL-on-FHIR-specific checks the generic validator above
# does not run.
vd-form-legend-live = Checked as you type: structure, cardinality, required bindings, and FHIRPath syntax / SQL-on-FHIR rules
# The guided form's validity chip (#843), client-side only, while the JSON
# editor's text does not parse at all — short like the chip's other two
# states ("No issues.", "3 issues"), never the longer editor-invalid-json
# sentence.
vd-form-invalid-chip = Invalid JSON
# SQL Query / SQL View's own three-line legend (#840/#1233): Save there
# gates the SQL on FHIR Library type and the SQL attachment, not the generic
# constraints/terminology promise `editor-legend-save` makes — a promise
# `HFS_VALIDATION_MODE` off (the default) would make false. The third line
# says where that attachment is actually edited, since this guided form
# never lists it (`hidden=["content"]`).
lib-form-legend-live = Checked as you type: structure, cardinality, required bindings
lib-form-legend-save = Checked on save: SQL on FHIR Library type and the SQL attachment
lib-form-legend-content = The SQL attachment (content) is not listed here: edit it in the SQL card below
editor-deferred-badge = on save
editor-deferred-hint = Codes are verified against the value set when you save (and live in the picker where a terminology server is configured)
editor-must-support-hint = Must-support: consumers of this profile are expected to handle this element
editor-add-filter = Filter elements and extensions
editor-add-another = add another
editor-pick-type = Pick a type…
editor-extension-url = Extension URL
editor-add-extension = Add extension

editor-valid = No issues.
editor-issues = { $count ->
    [one] { $count } issue
   *[other] { $count } issues
}

editor-modifier-badge = modifier
editor-modifier-warning = A modifier extension changes the meaning of this resource. A system that does not recognise it must refuse to process the resource.
editor-unknown-badge = not in schema
editor-unknown-hint = The schema does not describe this element. It is shown so it is not silently lost, and it is kept on save.

editor-primitive-extension-badge = + extension
editor-primitive-extension-hint = This value carries extensions of its own (a `_` sibling in the JSON). They are kept when you save.

editor-collapse-all = Collapse all
editor-expand-all = Expand all
json-view-toggle-fold = Toggle JSON section
editor-versions = Versions
editor-versions-none = No prior versions.

## Resources workspace (#282)

resources-heading = Resources
resources-lede = Browse, search, create, and edit FHIR resources. Search in natural language or build the query by hand, then open any result to edit it.
resources-create = Create new resource
resources-create-typed = Create new { $type }
resources-create-invalid-type = This resource type is not available in the selected FHIR version. Correct the query or choose a type from the list.
resources-create-not-advertised = This server does not allow creating this resource type. You can still search it.
resources-create-schema-unavailable = This resource type has no editor schema in the selected FHIR version, so the UI cannot create it safely.
resources-create-metadata-unavailable = Server capabilities are unavailable. Creation stays disabled until the UI can verify them.
resources-save-blocked = Fix the validation issues before saving.
resources-save-invalid = The JSON is not valid — fix it before saving.
resources-edit-title = Edit Resource
resources-tab-edit = Edit
resources-tab-history = History
resources-types-heading = Resource Types
rail-all-types-heading = All Types
# #1078: a type rail count from a snapshot counted from recent writes, not yet
# reconciled with storage. Hover title and screen-reader text of the "≈" count.
rail-count-approximate = Approximate: counted from recent writes and still being reconciled with storage.

queries-saved-group = Saved

nav-collapse = Collapse Menu
nav-menu-open = Open navigation menu
nav-menu-close = Close navigation menu
nav-main = Main navigation

batch-heading = Batch / Transaction
batch-lede = Upload a FHIR Bundle, review the actions it will run, execute it against this server, and read the outcome of every entry.
batch-upload = Upload
batch-drop-hint = Drop a bundle JSON file here
batch-drop-browse = or click to browse
batch-invalid-json = That file is not valid JSON.
batch-not-a-bundle = That JSON is not a FHIR Bundle.
batch-bad-type = Only Bundles of type batch or transaction can be executed here.
batch-request = Request
batch-entries = entries
batch-semantics-batch = Batch: entries run independently — a failed entry does not stop or undo the others.
batch-semantics-transaction = Transaction: all or nothing — if any entry fails, the server rolls the whole bundle back.
batch-tab-actions = Actions
batch-tab-json = Bundle JSON
batch-no-body = (no body — this entry only addresses a resource)
batch-cancel = Cancel
batch-execute = Execute
batch-plan-heading = Execution Plan
batch-done = Done
batch-response-heading = Per-Action Outcomes
batch-sum-created = created
batch-sum-updated = updated
batch-sum-other = read/other
batch-sum-failed = failed
batch-request-failed = The request failed.
batch-reading = Reading bundle…
batch-executing = Executing…
batch-read-failed = The file could not be read.

## Bulk Import workspace (#527)

bulk-import-title = Bulk Import
bulk-import-lede = Send precoordinated FHIR data sets to a Data Recipient with the Bulk Data $bulk-submit operation.
bulk-import-detail-lede = The manifests, status, and run log of this submission.
bulk-import-new = New Submission
bulk-import-create-title = Create Bulk Submission
bulk-import-field-name = Submission name
bulk-import-auth = Authentication
bulk-import-auth-none = None
bulk-import-auth-none-hint = No authorization header will be sent.
bulk-import-auth-backend = Backend services authentication
bulk-import-auth-backend-hint = Obtains an access token and sends it as a Bearer token in the authorization header.
bulk-import-field-client-id = Client ID
bulk-import-field-client-id-hint = Register this data provider with the Data Recipient and get back a client ID.
bulk-import-field-token-url = Token URL
bulk-import-field-token-url-hint = Authorization server's token endpoint URL.
bulk-import-jwks-hint = Register this server's public key with the recipient using the JWKS URL:
bulk-import-test-auth = Test authentication
bulk-import-test-auth-ok = Authentication succeeded.
bulk-import-create-submit = Submit
bulk-import-advanced = Advanced options
bulk-import-unavailable = The storage backend does not host the settings store, so submissions cannot be saved.
bulk-import-submissions = Submissions
bulk-import-records = records
bulk-import-col-name = Name
bulk-import-col-status = Status
bulk-import-col-created = Created
bulk-import-col-destination = Destination
bulk-import-empty = No submissions yet. Create one to get started.
bulk-import-all = All Submissions
bulk-import-status-not-started = Not Started
bulk-import-status-in-progress = In Progress
bulk-import-status-stopped = Stopped
bulk-import-status-completed = Completed
bulk-import-status-failed = Failed
# Persistent banner on a submission whose last Abort / Mark completed never
# reached the Data Recipient (#968): the submission is still running and the
# button can simply be pressed again. { $detail } is the recipient's own
# untranslated diagnosis — an HTTP status and reason, or a transport error.
bulk-import-status-error = The last status change did not reach the Data Recipient, so this submission is unchanged — try again. ({ $detail })
# The same banner while the change is still queued (#998): the recipient
# never answered (as opposed to refusing), so the request is kept and re-sent
# from the status card's own refresh for as long as this page stays open.
bulk-import-status-pending = The last status change did not reach the Data Recipient; it is kept and will be re-sent automatically while this page is open. ({ $detail })
bulk-import-detail-recipient = Data Recipient
bulk-import-detail-id = Submission ID
bulk-import-detail-submitter = Submitter
bulk-import-detail-created = Created
bulk-import-detail-status = Status
bulk-import-detail-auth = Authentication
bulk-import-abort = Abort
bulk-import-mark-completed = Mark completed
bulk-import-delete = Delete
bulk-import-edit = Edit
bulk-import-edit-title = Edit Submission
bulk-import-edit-submit = Save Changes
bulk-import-field-manifest-url = Manifest URL
bulk-import-field-manifest-url-hint = URL pointing to a Bulk Export Manifest with a precoordinated FHIR data set.
bulk-import-field-output-format = Format
bulk-import-field-output-format-hint = The format for the Bulk Data files in the manifest.
bulk-import-field-headers = File request headers
bulk-import-field-headers-hint = HTTP headers the Data Recipient should use when requesting a data file, one "Name: value" per line.
bulk-import-log = Submission Log
bulk-import-log-empty = Nothing submitted yet.
bulk-import-field-submitter-system = Submitter system
bulk-import-field-submitter-value = Submitter value
bulk-import-field-submitter-hint = Must match an identifier registered with the Data Recipient (coordinated out-of-band). Leave empty to use the generated defaults.
bulk-import-processing = Processing
bulk-import-processing-waiting = Waiting for the recipient's first status report…
bulk-import-result = Result
bulk-import-result-finished = Processing finished at
bulk-import-result-outputs = Output files
bulk-import-result-errors = Error files
bulk-import-result-warnings = Warnings
bulk-import-result-warnings-note = { $count ->
    [one] { $count } resource flagged with warnings (e.g. being re-indexed for search)
   *[other] { $count } resources flagged with warnings (e.g. being re-indexed for search)
}
ui-cancel = Cancel
ui-close = Close
ui-combobox-selected-label = Selected items
ui-combobox-remove = Remove
ui-combobox-added = Added
ui-combobox-removed = Removed
ui-combobox-loading = Loading suggestions…
ui-combobox-results-updated = Suggestions available:
ui-combobox-error = Suggestions could not be loaded. Try again.
ui-combobox-name-now = now { $name }
ui-combobox-footer-total = { $count } matches · scroll for more
ui-combobox-footer-more = More matches · scroll for more
ui-combobox-footer-end = End of results
ui-combobox-loading-more = Loading more matches…
editor-orphans-title = These issues have no field yet — add the elements to fix them
editor-hint-date = FHIR date: YYYY, YYYY-MM, or YYYY-MM-DD
editor-hint-datetime = FHIR dateTime: YYYY, YYYY-MM, YYYY-MM-DD, or a full timestamp with timezone (2024-05-17T14:30:00+02:00)
editor-hint-time = FHIR time: HH:MM:SS
editor-hint-instant = FHIR instant: full timestamp with timezone, e.g. 2024-05-17T14:30:00.000Z

## Subscriptions operator page (#580)

subs-title = Subscriptions
subs-lede = Read-only view of the subscriptions engine: every registered subscription, its channel, live status, and delivery counters.
subs-unavailable = The subscriptions engine is not enabled on this server.
subs-unavailable-how = Turn it on by starting HFS with:
subs-empty = No subscriptions are registered for this tenant.
subs-card-failing = Failing
subs-card-failing-sub = Needs attention
subs-card-idle = Idle
subs-card-idle-sub = No clients
subs-card-active = Active
subs-card-active-sub = delivering
subs-card-delivered = Delivered in 24 h
subs-card-delivered-sub = { $rate }% first try
subs-card-delivered-none = no deliveries in the window
subs-table-heading = Subscriptions
subs-sort = Sort
subs-sort-status = Status
subs-sort-sent = Most sent
subs-sort-fails = Fail streak
subs-col-subscription = Subscription
subs-col-channel = Channel
subs-col-status = Status
subs-col-last24 = Last 24 hrs
subs-col-sent = Sent
subs-col-fails = Fail streak
subs-state-active = Active
subs-state-error = Error
subs-state-idle = 0 clients
subs-state-requested = Requested
subs-state-off = Off

## Bulk Export workspace (#537)

bulk-export-title = Bulk Export
bulk-export-lede = Pull data out of this server as NDJSON files with the FHIR Bulk Data $export operation.
bulk-export-active-title = Exports
bulk-export-new = New Export
bulk-export-unavailable = The storage backend does not host the settings store, so export jobs cannot be tracked.
bulk-export-scope = What are you exporting?
bulk-export-scope-system = Everything
bulk-export-scope-system-hint = The whole server — every resource type you select below.
bulk-export-scope-patient = Patients
bulk-export-scope-patient-hint = Every patient, or only the ones you pick, with the records that belong to them. Nothing patient-unrelated.
bulk-export-scope-group = Group
bulk-export-scope-group-hint = Just the members of a cohort you've already defined.
bulk-export-field-group-id = Group ID
bulk-export-field-group-id-hint = Required for the Group scope: the id of the FHIR Group to export.
bulk-export-field-patients = Patients
bulk-export-field-patients-placeholder = Search patients
bulk-export-field-patients-hint = Search by name, surname or exact identifier. Leave empty to export every patient.
bulk-export-field-patients-fallback-placeholder = Patient FHIR IDs
bulk-export-field-patients-fallback-hint = Enter exact logical FHIR IDs separated by commas or new lines. Leave empty to export every patient.
bulk-export-field-patients-id-only-hint = Search by exact FHIR ID. Leave empty to export every patient.
bulk-export-patient-options-empty = No matching patients found.
bulk-export-patient-invalid = Enter only valid logical Patient IDs, separated by commas or new lines.
bulk-export-field-name = Name
bulk-export-field-name-placeholder = Diabetes registry 2024
bulk-export-name-required = Enter a name for this export.
bulk-export-types = Resource types
bulk-export-all-resources = All Resources
bulk-export-narrow = Narrow it down
bulk-export-field-elements = FHIR elements
bulk-export-field-type-filter = Type filter
bulk-export-field-since = Since
bulk-export-since-all = All time
bulk-export-since-day = Last day
bulk-export-since-week = Last 7 days
bulk-export-since-month = Last 4 weeks
bulk-export-since-custom = Custom
bulk-export-field-since-custom = Custom instant
bulk-export-since-invalid = Enter a valid FHIR instant, such as 2026-08-01T00:00:00Z.
bulk-export-field-until = Until
bulk-export-field-until-hint = Optional upper bound. RFC 3339, e.g. 2026-08-01T00:00:00Z.
bulk-export-until-before-since = Until must not be earlier than Since.
bulk-export-window-since = Since
bulk-export-window-until = Until
bulk-export-start = Start Export
bulk-export-clear = Clear
bulk-export-files-count = { $count ->
    [one] { $count } file
   *[other] { $count } files
}
bulk-export-summary-count = { $count ->
    [one] { $count } export
   *[other] { $count } exports
} · { $running } running
bulk-export-none = No exports yet. Use New Export to start one.
bulk-export-status-in-progress = In progress
bulk-export-status-complete = Complete
bulk-export-status-failed = Failed
bulk-export-status-cancelled = Cancelled
bulk-export-progress = Progress
bulk-export-progress-waiting = Waiting for the first status report…
bulk-export-writing = Writing { $name }
bulk-export-types-progress = { $done } of { $total } types
bulk-export-files = Files
bulk-export-finished-in = finished in
bulk-export-error = Error
bulk-export-cancel = Cancel
bulk-export-cancel-refused = The server refused the cancel:
bulk-export-retry = Retry
bulk-export-run-again = Run again
bulk-export-download-all = Download All Resources
bulk-export-download-all-aria = Download all resources from { $name }
bulk-export-view-files = View files
bulk-export-more-actions = More actions
bulk-export-delete = Delete
bulk-export-delete-aria = Delete export { $name }
bulk-export-delete-warning = Delete { $name } and its output files from the server? This cannot be undone.
bulk-export-delete-confirm = Delete export
bulk-export-delete-cancel = Keep export
bulk-export-delete-error = The export could not be deleted safely. Its card was kept so you can try again.
bulk-export-detail-job-heading = Job
bulk-export-detail-field-scope = Scope
bulk-export-detail-field-started = Started
bulk-export-detail-field-duration = Duration
bulk-export-detail-outputs-heading = Output files
bulk-export-detail-col-type = Resource type
bulk-export-detail-col-files = Files
bulk-export-detail-outputs-empty = The export produced no output files.

# CapabilityStatement page (#653)
cap-title = Capability Statement
cap-lede = What this server does right now, for the selected tenant and FHIR version — composed live from /metadata.
cap-summary-heading = Server Summary
cap-summary-description = Description
cap-summary-url = Base URL
cap-summary-fhir-version = FHIR version
cap-summary-status = Status
cap-summary-kind = Kind
cap-summary-date = Date
cap-summary-formats = Formats
cap-interactions-heading = System Interactions
cap-transaction-note = transaction is advertised because the active backend supports atomic transactions; batch is always available.
cap-role-matrix = View the backend role matrix.
cap-operations-heading = Operations
cap-col-operation = Operation
cap-col-definition = Definition
cap-resources-heading = Per-Resource Capabilities
cap-filter-placeholder = Filter types…
cap-col-type = Type
cap-col-interactions = Interactions
cap-col-search-params = Search params
cap-col-includes = Includes
cap-col-revincludes = Revincludes
cap-resources-empty = No resource types match the filter.
cap-raw-toggle = Raw CapabilityStatement (JSON)
cap-raw-load = Open plain JSON
cap-raw-loading = Loading highlighted JSON…
cap-raw-plain = Plain JSON fallback. Highlighting is loaded incrementally when JavaScript is available.
cap-json-load-error = This JSON section could not be loaded. Collapse and reopen it to retry.
cap-json-truncated = display truncated
cap-json-path-too-deep = maximum expansion depth reached
cap-json-pagination-label = JSON items
cap-json-page-prev = Previous
cap-json-page-next = Next
cap-json-collapse-all = Collapse all
cap-json-expand-all = Expand all
cap-json-expanding = Expanding visible JSON…
cap-json-expand-complete = All JSON is expanded.
cap-json-expand-partial = Expanded the current pages up to the safe limit. Remaining nodes stay collapsed.
cap-json-limit-reached = The JSON display limit was reached. Collapse another branch before loading more.
cap-json-expand-error = The JSON tree could not be expanded. The current view was kept; try again.
cap-unavailable = The CapabilityStatement could not be fetched from the server — the self-call may need an outbound token when authentication is enabled.

## Generic JSON fold labels (#898) — used by raw response folds in HTS-UI.
## The cap-* keys above are kept for CapabilityStatement specifically.
json-fold-loading = Loading JSON…
json-fold-load-error = This JSON section could not be loaded. Collapse and reopen it to retry.
json-fold-open-plain = Open plain JSON
json-fold-plain = Plain JSON fallback. Highlighting is loaded incrementally when JavaScript is available.

## SQL on FHIR section stubs (#649)


sql-vd-title = View Definitions
sql-vd-lede = Author and manage the ViewDefinitions that SQL on FHIR runs flatten resources with.

sql-queries-title = SQL Queries
sql-queries-lede = Ad-hoc SQL over the flat tables your view definitions produce. Results refresh as you type.

sql-views-title = SQL Views
sql-views-lede = Named views other queries and exports select from. Define the SQL once; the columns it produces become a table everyone else can use.

sql-export-title = SQL Export
sql-export-lede = Run stored view definitions, SQL queries and SQL views as a long-running $sql-export job and download the output files.
sql-export-active-title = SQL Exports

## View Definitions workspace (#649)

vd-new = Create New
vd-new-title = New View Definition
vd-rail-label = View definitions
vd-rail-heading = View Definitions
vd-rail-all-heading = All View Definitions
vd-filter = Filter views
vd-none = No view definitions yet.
vd-empty-lede = Create your first ViewDefinition with Create New.
vd-degraded = The view definition list could not be loaded.
vd-saved = Saved.
vd-run-failed = Could not run the view.
vd-save = Save
vd-duplicate = Duplicate
sql-duplicate-read-failed = Could not check existing artifacts before duplicating: { $reason }
sql-duplicate-incomplete = Could not check all existing artifacts. The copy was not saved; try again after the catalog can be fully read.
vd-delete = Delete
vd-delete-confirm = Delete view definition "{ $name }"? This cannot be undone.
vd-delete-failed = Could not delete the view definition.
vd-json-heading = Definition (JSON)
vd-run-hint = Runs as you type — results follow the current definition, saved or not
vd-results-heading = Results
vd-results-empty = The view produced no rows.
# #752: the $sql-run preview's row/duration meta, shared by the page's own
# render and the `/run` fragment endpoint.
vd-results-meta = { $rows } rows · { $ms } ms
# #752: the results meta after a failed run — the previous table stays on
# screen, relabelled.
vd-results-stale = last successful run
vd-results-running = Running query…
vd-pagination-label = View definition pages
vd-page-prev = Previous
vd-page-next = Next

## ViewDefinition lint messages and fixes (#821): the `/lint` handler's own
## localization of `helios_sof::lint::Diagnostic` — code + args here, never
## `Diagnostic.message` (English-only, unlocalized) itself. `vd-lint-*` keys
## are named after `DiagnosticCode`'s own kebab-case wire form; a
## `missing-required`/`wrong-type` diagnostic from a constant's value[x]
## choice carries `$variant` ("missing"/"multiple") instead of the generic
## `$key`/`$expected`+`$found` args, selected on below. `vd-fix-*` keys label
## one `Fix` each, shown next to the diagnostic as a one-click action.
vd-lint-not-a-view-definition = Not a ViewDefinition: found { $found }
vd-lint-unknown-key = Unknown key "{ $key }"
vd-lint-missing-required = { $variant ->
    [missing] A constant must set exactly one value
   *[other] Missing required key "{ $key }"
}
vd-lint-wrong-type = { $variant ->
    [multiple] A constant may set only one value
   *[other] Expected { $expected }, found { $found }
}
vd-lint-empty-required = Required key "{ $key }" must not be empty
vd-lint-duplicate-column-name = Duplicate column name "{ $name }"
vd-lint-multiple-iteration-directives = A select may set at most one of forEach, forEachOrNull, repeat (found { $keys })
vd-lint-select-without-output = A select must have at least one of column, select, or unionAll
vd-lint-fhirpath-syntax = FHIRPath syntax: { $detail }
vd-lint-undeclared-constant = Undeclared constant "%{ $name }"
vd-lint-unknown-resource-type = Unknown resource type "{ $found }"
vd-fix-rename-key = Rename to "{ $to }"
vd-fix-remove-key = Remove "{ $key }"
vd-fix-set-string = Set to "{ $value }"

## ViewDefinition completion (#821): `vd-editor.js` reads this once off
## `#vd-editor-grid`'s own `data-msg-required` — the marker a required
## structural key's completion item carries, appended to its `detail` text
## client-side (never sent by `/complete` itself, which is not locale-aware).
vd-complete-required = required

## `data-msg-quickfix` on `#vd-editor-grid`: the accessible name of the quick-fix
## menu Ctrl+. opens next to the cursor when several fixes apply.
vd-quickfix-menu = Quick fixes

## ViewDefinition editor save-with-errors confirmation (#821): `vd-editor.js`
## picks whichever of these renders `#vd-editor-grid`'s own
## `data-msg-save-errors-one`/`-other` and substitutes the real count for the
## literal `{count}` marker below, itself the value `$count` was given —
## `window.confirm`'s own text has no locale of its own, so this is the only
## way to translate it. Shown only when submitting as Save (not Duplicate)
## while the most recently completed lint pass still has at least one error.
vd-save-with-errors-one = This view definition still has { $count } error. Save it anyway?
vd-save-with-errors-other = This view definition still has { $count } errors. Save it anyway?

## $sql-run results partial (#752): shared by the View Definitions, SQL
## Queries, and SQL Views playgrounds (#839).

sql-run-export = Export as files

## SQL Queries / SQL Views workspaces (#649, editor-first layout #839)

sql-queries-new-title = New SQL Query
sql-views-new-title = New SQL View
sql-queries-rail-all-heading = All SQL Queries
sql-views-rail-all-heading = All SQL Views
# The title row's `.tag--type` chip (#839) — one document's type, singular,
# distinct from the *-title keys above (the collection's own plural name).
sql-queries-chip = SQL Query
sql-views-chip = SQL View
sql-queries-filter = Filter queries
sql-views-filter = Filter views
sql-queries-rail-empty = No queries yet.
sql-views-rail-empty = No views yet.
sql-rail-no-match = No matches for “{ $filter }”.
sql-rail-clear-filter = Clear the filter
sql-rail-selection-filtered = Not in the list filtered by “{ $filter }”.
sql-queries-empty-title = No SQL queries yet
sql-views-empty-title = No SQL views yet
sql-queries-empty-lede = Write your first query with Create New. It runs against the flat tables of every active view definition.
sql-views-empty-lede = Define your first view with Create New. Its columns become a table other queries and exports can select from.
sql-queries-editor-heading = SQL
sql-views-editor-heading = View definition (SQL)
sql-queries-results-heading = Results
sql-views-results-heading = Preview
sql-queries-run-failed = Could not run the query.
sql-views-run-failed = Could not run the view.
lib-degraded = The library list could not be loaded.
# The SQL card's "runs as you type" legend (#839) — shared verbatim by SQL
# Queries and SQL Views, unlike the headings/failure prefix above.
lib-run-hint = Runs as you type — results follow the current SQL, saved or not
# The SQL card's own notice (#1233), shown next to the legend above only
# while the Details JSON's `application/sql` attachment does not decode
# (invalid base64, non-UTF-8 bytes, or a missing `data`) — the card keeps
# showing its last readable text rather than clearing it, and typing there
# repairs the attachment. `sql-library-sync.js` is the only thing that ever
# shows it; the server always paints it hidden.
lib-sql-attachment-unreadable = SQL attachment unreadable: the SQL card keeps its last readable text; typing here repairs it
lib-delete-confirm = Delete "{ $name }"? This cannot be undone.
lib-delete-failed = Could not delete the library.
# Details section (#840): the full stored Library (SQL attachment
# included, #1233), edited as JSON (left) and through the guided form
# (right).
lib-details-heading = Details
lib-details-json-heading = Library (JSON)
lib-details-json-note = The SQL attachment (content[].data) is part of this document. The SQL card below edits the same attachment.
# Shown only for `?lib=new`, under the Details heading — closes the #839
# follow-up asking for a hint about the starter's `change-me` placeholder.
lib-details-new-lede = Rename it and point relatedArtifact[0] at a ViewDefinition that exists.
# The Save gate (#840): rejects a document whose `type.coding` names the
# other kind's code (a sql-view saved from SQL Queries, or the reverse),
# which would otherwise silently vanish it from the rail it was just edited
# on. `$code` is the route's own expected code ("sql-query"/"sql-view").
lib-save-wrong-kind = The Library's SQL on FHIR type must be "{ $code }" to save it here.
# The Save gate's SQL View counterpart (#841): the SQLView profile
# fixes `Library.parameter` to 0..0, so a non-empty declaration is rejected
# the same way the wrong `type.coding` above is. The identical text also
# backs the `/run` fragment's own pre-`$sql-run` check for the same case —
# one message, two call sites.
lib-save-view-parameters = A SQL View cannot declare parameters (SQLView profile, Library.parameter 0..0). Remove parameter[] in Details.

## Parameters card (#841, SQL Query only)

lib-params-heading = Parameters
lib-params-meta = declared on the Library · values below are for this run only
lib-params-empty = No parameters declared. Add one here or in Details.
# `$name` is the bare placeholder name, without its leading `:` — both
# messages below add it themselves so a translation can place it anywhere
# in the sentence.
lib-params-hint = :{ $name } is used in the SQL but not declared on the Library.
lib-params-declare = Declare :{ $name }
lib-params-add-toggle = Add parameter
lib-params-add-name-label = Name
lib-params-add-type-label = Type
lib-params-add-submit = Add
# `add-parameter`'s own validation messages (#841) — type codes
# (`string`, `integer`, …) are never translated.
lib-params-add-invalid-name = Parameter names must match ^[A-Za-z][A-Za-z0-9_]*$
lib-params-add-duplicate = A parameter named { $name } is already declared
lib-params-add-unknown-type = Unknown parameter type
# The live-run "waiting for a value" notice (#841): `$names` is
# already comma-joined with each name's own leading `:` by the caller (one
# name, or several), since a Fluent list can't format an arbitrary-length
# collection on its own.
lib-run-waiting = Waiting for a value for { $names } — the results below are from the last successful run.

# The unknown-table lint's own live-run notice (#842/04): the SQL reads a
# table no declared `relatedArtifact[depends-on]` label names, so `$sql-run`
# is never called. `lib-run-unknown-table` is the *first* unknown table's
# own full sentence (the notice's own banner text when there is only one);
# `lib-run-unknown-table-more` is the short form every additional table
# (and every table's own hover tooltip in the editor) uses instead. `$line`
# is the table's own 1-based line number.
lib-run-unknown-table = Unknown table { $name } — line { $line }. Declare it under Reads from or fix the name. Your SQL is unchanged; the results below are from the last successful run.
lib-run-unknown-table-more = Unknown table { $name } — line { $line }.

## Tables panel (#842, both kinds) — Reads from / Used by / Columns

lib-tables-heading = Reads from
# The `<code>` cell's own placeholder for a `relatedArtifact[depends-on]`
# entry with no `label` at all — a malformed document, most commonly one
# hand-edited in Details.
lib-tables-no-alias = (no label)
lib-tables-empty = No tables declared yet.
# The resolved-target chip text. "SQL View" reuses `sql-views-chip` — the
# same chip the title row and *Used by* already show for that kind.
lib-tables-kind-view-definition = ViewDefinition
lib-tables-kind-value-set = ValueSet
lib-tables-kind-concept-map = ConceptMap
lib-tables-target-not-found = Not found
lib-tables-target-not-found-detail = No ViewDefinition, SQL View, ValueSet or ConceptMap answers to { $resource }. Fix the canonical in Details or remove the row.
lib-tables-target-not-a-table = Not a table
lib-tables-target-not-a-table-detail = Only a ViewDefinition, a SQL View, a ValueSet or a ConceptMap can be read.
lib-tables-add-table-label = Table
# The always-visible add row's own search field placeholder (#1238).
lib-tables-add-table-placeholder = Add a table…
lib-tables-add-table-fallback-placeholder = ViewDefinition/{"{"}id{"}"} or Library/{"{"}id{"}"}
lib-tables-alias-label = Alias
lib-tables-add-submit = Add
# The add row's own single help line (#1238) — the only hint the row
# carries; the combobox's own `hint`/`fallback-hint` are passed empty.
lib-tables-add-hint = Search a view definition or SQL view; the alias defaults to its name.
# `add-table`'s own validation messages (#842), in the order they are
# checked.
lib-tables-add-error-required = Pick a view definition or SQL view
lib-tables-add-error-alias-required = Alias is required
lib-tables-add-error-alias-invalid = Alias must match ^[A-Za-z][A-Za-z0-9_]*$
lib-tables-add-error-alias-duplicate = Alias { $alias } is already declared
# The *Add table* combobox's own "no matches" result message
# (`partials/lookup_options.html`), mirroring `bulk-export-patient-options-
# empty`/`sql-export-group-options-empty` for the two existing pickers.
lib-tables-options-empty = No matches.
# The unknown-table lint's own row (#842/04): a table the SQL reads that no
# dependency declares, appended after every resolved row.
lib-tables-unknown = Unknown table
lib-tables-unknown-detail = Used in the SQL but not declared. Pick a view definition or SQL view, or fix the name.
lib-tables-declare = Declare { $name }
# The row's own visible *Declare* button text (#1238) — the accessible
# name carries the table's own name instead (`lib-tables-declare`).
lib-tables-declare-short = Declare
# The row's own icon *Remove* button's accessible name (#1238) — `$alias`
# is the alias exactly as the `<code>` cell shows it, including the
# `lib-tables-no-alias` placeholder when the row has none.
lib-tables-remove-row = Remove { $alias }

lib-used-by-heading = Used by
lib-used-by-empty = Nothing uses this yet.
lib-used-by-export-kind = SQL Export

lib-columns-heading = Columns
lib-columns-meta-query = what the query produces
lib-columns-meta-view = what the view produces
lib-columns-empty-query = Run the query to see its columns.
lib-columns-empty-view = Run the view to see its columns.
lib-columns-col-name = Column
lib-columns-col-type = Type
lib-columns-col-from = From

## SQL Export pages (#649, #833)

export-start-failed = Could not start the export.
export-format = Output format
export-start = Start Export

## SQL Export builder — create form (#834)

sql-export-name-placeholder = Monthly patient flat files
sql-export-subjects-legend = What are you exporting?
sql-export-subjects-heading = Subjects
sql-export-filter-all = All
sql-export-filter-view-definitions = View Definitions
sql-export-filter-queries = Queries
sql-export-filter-views = Views
sql-export-filter-subjects = Filter subjects
sql-export-select-all = Select all
sql-export-col-subject = Subject
sql-export-col-kind = Kind
sql-export-col-status = Status
sql-export-selected-count = { $selected } of { $total } selected
sql-export-format-ndjson-hint = One JSON object per row. Streams well into pipelines.
sql-export-format-csv-hint = Header row plus one line per row. Opens in any spreadsheet.
sql-export-format-json-hint = A single JSON array per output. Easiest to inspect by hand.
sql-export-format-parquet-hint = Columnar and typed. Best for analytics engines.
sql-export-empty-heading = Nothing to export yet
sql-export-empty-body = Every stored view definition, SQL query and SQL view shows up here as an export subject. Create one first.
sql-export-filter-empty = No subjects match

## SQL Export builder — "Narrow it down" and "Advanced" (#836)

sql-export-narrow-title = Narrow it down
sql-export-narrow-meta = optional — leave empty to export everything
sql-export-field-patients = Patients
sql-export-field-patients-placeholder = Search patients
sql-export-field-patients-hint = Search by name, surname or exact identifier. Leave empty to export every patient.
sql-export-field-patients-fallback-placeholder = Patient FHIR IDs
sql-export-field-patients-fallback-hint = Enter exact logical FHIR IDs separated by commas or new lines. Leave empty to export every patient.
sql-export-field-patients-id-only-hint = Search by exact FHIR ID. Leave empty to export every patient.
sql-export-field-groups = Groups
sql-export-field-groups-placeholder = Search groups
sql-export-field-groups-hint-r4 = Search by exact FHIR ID or identifier.
sql-export-field-groups-hint-r5 = Search by name, exact FHIR ID or identifier.
sql-export-field-groups-fallback-placeholder = Group FHIR IDs
sql-export-field-groups-fallback-hint = Enter exact FHIR IDs or identifiers separated by commas or new lines.
sql-export-group-options-empty = No matching groups found.
sql-export-field-since = Since
sql-export-since-all = All time
sql-export-since-day = Last day
sql-export-since-week = Last 7 days
sql-export-since-month = Last 4 weeks
sql-export-since-custom = Custom
sql-export-field-since-custom = Custom instant
sql-export-since-invalid = Enter a valid FHIR instant, such as 2026-08-01T00:00:00Z.
sql-export-patient-invalid = Enter only valid logical Patient IDs, separated by commas or new lines.
sql-export-group-invalid = Enter only valid logical Group IDs, separated by commas or new lines.
sql-export-advanced = Advanced
sql-export-field-tracking-id = Tracking id
sql-export-field-tracking-id-hint = Optional label for your own records, for any format. Shown on the export's details page and returned as clientTrackingId when the export completes.
sql-export-tracking-id-too-long = Tracking id must be 200 characters or fewer.
sql-export-field-header = Include a header row
sql-export-field-header-hint = (CSV only — ignored for other formats)

## Active SQL Exports list and job cards (#833)

sql-export-new = New SQL Export
sql-export-unavailable = The storage backend does not host the settings store, so SQL export jobs cannot be tracked.
sql-export-none = No SQL exports yet. Use New SQL Export to start one.
sql-export-store-error = The export started, but could not be added to this list. Job id:
sql-export-summary-count = { $count ->
    [one] { $count } export
   *[other] { $count } exports
} · { $running } running
sql-export-select-subject = Select at least one subject.
sql-export-unknown-subject = One or more selected subjects are no longer available. Refresh the page and try again.
sql-export-cancelled-reason = the server no longer knows this job
sql-export-status-in-progress = In progress
sql-export-status-complete = Complete
sql-export-status-failed = Failed
sql-export-status-cancelled = Cancelled
sql-export-view-files = View files
sql-export-cancel = Cancel
sql-export-retry = Retry
sql-export-run-again = Run again
sql-export-remove = Remove from list
sql-export-more-actions = More actions
sql-export-copy-job-id = Copy job id
sql-export-copied = Copied
sql-export-progress-waiting = Waiting for the first status report…
sql-export-writing = Writing { $name }
sql-export-subjects-progress = { $total ->
    [one] { $done } of { $total } subject
   *[other] { $done } of { $total } subjects
}
sql-export-started = started
sql-export-finished-in = finished in
sql-export-cancelled-at = cancelled at
sql-export-status-unavailable = status unavailable
sql-export-format-ndjson = NDJSON
sql-export-format-csv = CSV
sql-export-format-json = JSON
sql-export-format-parquet = Parquet
sql-export-subjects-count = { $count ->
    [one] { $count } subject
   *[other] { $count } subjects
}
sql-export-kind-view-definition = { $count ->
    [one] { $count } ViewDefinition
   *[other] { $count } ViewDefinitions
}
sql-export-kind-sql-query = { $count ->
    [one] { $count } SQL Query
   *[other] { $count } SQL Queries
}
sql-export-kind-sql-view = { $count ->
    [one] { $count } SQL View
   *[other] { $count } SQL Views
}
sql-export-files-count = { $count ->
    [one] { $count } file
   *[other] { $count } files
}

## Job detail page (#835)

sql-export-detail-finished = Finished
sql-export-detail-failed = Failed
sql-export-detail-cancelled = Cancelled
sql-export-detail-started = Started
sql-export-detail-after = after
sql-export-failure-subject = The export stopped on subject
sql-export-failure-generic = The export failed:
sql-export-detail-job-heading = Job
sql-export-detail-field-job-id = Job id
sql-export-detail-field-format = Format
sql-export-detail-field-started = Started
sql-export-detail-field-duration = Duration
sql-export-detail-field-subjects = Subjects
sql-export-detail-header-included = with header row
sql-export-detail-header-omitted = no header row
sql-export-detail-outputs-heading = Output files
sql-export-detail-col-output = Output
sql-export-detail-col-subject = Subject
sql-export-detail-col-files = Files
sql-export-detail-outputs-empty = The job produced no output files.

## #837: per-SQL-Query parameter values on the SQL Export builder
sql-export-param-count = { $count ->
    [one] { $count } parameter
   *[other] { $count } parameters
}
sql-export-param-required = This value is required.
sql-export-param-type-mismatch = Expected a { $type } value.
sql-export-param-required-chip = required
sql-export-toggle-values = Show or hide values
sql-export-values-missing-one = { $count } value missing
sql-export-values-missing-other = { $count } values missing

## partials/sql_parameter_fields.html (#837) — shared with the SQL Query
## page's own parameter form
sql-param-default = default

## HTS administrative UI (crates/hts-ui) — Phase 1 scaffold stubs
##
## Keys for the HTS UI follow the convention hts-<page>-<role>-<control>. These stubs cover the base layout, sidebar
## nav, and the dashboard scaffold placeholder rendered by the Phase 1 blocker
## slice. They must be kept in parity with es/de/main.ftl.

-hts-app-name = HealthyMind Terminology Server




## Dashboard rows (row headings are visually hidden — they're for screen readers).


## Dashboard tiles.


## Home request-rate chart (design doc §7.1). Plots a rate differenced from
## the cumulative `/metrics` counters, sampled only while this page is open,
## so several honest "nothing to draw" states need their own copy.


## /health `status` values, keyed for translation.


## Degraded banner (design doc §7 header contract).


## Dialect chip (topbar, session-wide displayLanguage / Accept-Language per §7.1).


## OperationOutcome partial (shared, design doc §7 / §11).


## HTS Slice B — CodeSystem browser + detail with embedded workbench
## (design doc §7.2 + §7.3). Every key here has a peer in es/de/main.ftl.

## CodeSystem status pills (used by browser rows and detail header).


## CS browser page.


## Phase 5 — HTS search-form shared strings (used by CS / VS / CM browsers).


## CS detail page.


## CS $lookup form + result labels.


## CS $validate-code form + result labels.


## CS $subsumes form + result labels.


## Shared workbench chrome (reused by Slice C/D/E workbenches).


## Additional degraded reason for CS-read 404s (design doc §7.3 states matrix).


## HTS Slice C — ValueSet browser + detail with embedded $expand workbench
## (design doc §7.4 + §7.4.1). Every key here has a peer in es/de/main.ftl.

## ValueSet status pills.


## VS browser page.


## VS detail page.


## VS $expand form + result labels.


## HTS Slice D — ConceptMap browser + detail with embedded $translate
## workbench (design doc §7.5). Every key here has a peer in
## es/de/main.ftl.

## ConceptMap status pills.


## CM browser page.


## CM detail page.


## CM $translate form + result labels.


## HTS Slice E -- standalone Operations workbench (design doc s7.6).
## Every user-visible string on `/ui/hts/operations` resolves to a key
## in this section. Keys have peers in es/de/main.ftl (parity gated by
## the fluent-key inventory test).

## Shell.

## Op selector labels -- one per OperationKind slug.

## CS $lookup widening (Slice E adds useSupplement to the Slice B set).

## CS $validate-code widening.

## CS $subsumes standalone (heading + outcomes already live in Slice B).

## VS $expand widening (adds designation chip).

## VS $validate-code (new op in Slice E).

## CM $translate standalone (base keys already live in Slice D).

## $closure workbench (new op in Slice E).

## batch-validate workbench (new UI-fabricated op in Slice E).

## Slice F — standalone Import page (design doc §7.7).
##
## Shell + upload form + status region for POST /import. All strings
## live under `hts-import-*`; `hts-nav-import` above is the sidebar
## label reused from the Phase 1 stub set.



## Slice G — standalone Diagnostics page (design doc §7.9).
##
## Stacked-card view over CapabilityStatement, TerminologyCapabilities,
## /health, and /metrics — mirrors HFS's Capability Statement page.
## A failing source renders an `hts-degraded-reason-*` warning notice
## inside its own card; the other cards stay readable.






## HTS detail pages -- V3 compact header (#551 Slice B/C/D layout pass).
## Shared chip-row and disclosure labels for the CodeSystem / ValueSet /
## ConceptMap detail pages, plus the result-panel headings and the two
## honesty footnotes (tree pager, reverse-mode originMap).






## Bearer-only authentication (#1560): auth is on, no browser sign-in configured
auth-bearer-only = Authentication is enabled on this server, but no browser sign-in is configured. Pages that call the FHIR API from the browser — Resources, Batch / Transaction, saved queries and preferences — are refused with 401. Set HFS_UI_LOGIN_CLIENT_ID to enable the sign-in, or use the API directly with a bearer token.
batch-sign-in-required = This server has no browser sign-in configured, so this page cannot authenticate the request. Set HFS_UI_LOGIN_CLIENT_ID to execute bundles from here.
batch-too-large = The bundle is larger than this server accepts. Split it into smaller bundles, or raise HFS_MAX_BODY_SIZE on the server.

## Batch / Transaction: paste mode and client-side checks (p1-ui)
batch-mode-label = Bundle input
batch-mode-file = Upload file
batch-mode-paste = Paste JSON
batch-paste-label = Bundle JSON
batch-paste-placeholder = Paste a batch or transaction Bundle here…
batch-paste-hint = Checked as you type; every problem names its line and column.
batch-format = Format
batch-format-failed = Fix the JSON syntax error before formatting.
batch-continue = Review plan
batch-check-ok = No problems found.
batch-check-errors = Errors
batch-check-warnings = Warnings
batch-check-by-method = By method
batch-check-by-type = By resource type
batch-check-entries = %count% entries in total
batch-check-blocked = Fix the errors to continue. Warnings do not block.
batch-check-at = line %line%, col %col%

## Shared bundle checks (bundle-check.js). %name% placeholders are filled in the browser.
bundle-check-empty = Nothing to check yet: paste or upload a Bundle.
bundle-check-json-syntax = JSON syntax error: %detail%.
bundle-check-json-eof = the text ends before the JSON is complete
bundle-check-json-unexpected = unexpected character %char%
bundle-check-json-expected = expected %expected%
bundle-check-not-object = The JSON must be a single resource object.
bundle-check-not-bundle = resourceType is “%found%”; a batch or transaction must be a Bundle.
bundle-check-missing-type = Bundle.type is missing: set it to batch or transaction.
bundle-check-bad-type = Bundle.type is “%found%”. Only batch and transaction bundles can be submitted, because only they carry a request (method and url) for every entry.
bundle-check-entry-not-array = Bundle.entry must be an array.
bundle-check-no-entries = The bundle has no entries, so nothing would run.
bundle-check-entry-not-object = Entry %entry% is not a JSON object.
bundle-check-no-request = Entry %entry% has no request: every batch or transaction entry needs request.method and request.url.
bundle-check-no-method = Entry %entry%: request.method is missing.
bundle-check-bad-method = Entry %entry%: request.method “%method%” is not one of GET, POST, PUT, PATCH, DELETE, HEAD.
bundle-check-no-url = Entry %entry%: request.url is missing.
bundle-check-bad-url = Entry %entry%: a %method% request.url must look like Type/id (or Type?search for a conditional %method%), not “%url%”.
bundle-check-no-resource = Entry %entry%: a %method% entry needs a resource.
bundle-check-no-resource-type = Entry %entry%: the resource has no resourceType.
bundle-check-url-type-mismatch = Entry %entry%: request.url targets %url_type%, but the resource is a %type%.
bundle-check-id-mismatch = Entry %entry%: request.url names id “%url_id%”, but resource.id is “%id%”.
bundle-check-duplicate-full-url = Entry %entry%: fullUrl %fullurl% is also used by entry %other%.
bundle-check-unresolved-urn = Entry %entry%: reference %ref% matches no entry’s fullUrl in this transaction.
bundle-check-too-large = The bundle is %size%, larger than this server accepts (%limit%). Split it, or raise HFS_MAX_BODY_SIZE on the server.

## Validate without importing ($validate), Batch page and Validate page
nav-validate = Validate
validate-run = Validate
validate-run-hint = Checks the bundle on the server ($validate) without importing anything.
validate-per-entry = Validate each entry individually
validate-per-entry-hint = Also runs $validate on every entry’s resource, against its own meta.profile.
validate-running = Validating…
validate-progress = Validating entry %done% of %total%…
validate-result-heading = Validation result
validate-heading-bundle = Bundle
validate-heading-resource = Resource
validate-heading-entries = Entries, validated individually
validate-verdict-ok = Valid: no errors or warnings
validate-verdict-errors = Errors: %errors% · Warnings: %warnings%
validate-verdict-warnings = Valid, with warnings: %warnings%
validate-group-error = Errors
validate-group-warning = Warnings
validate-group-information = Information
validate-sev-fatal = Fatal
validate-sev-error = Error
validate-sev-warning = Warning
validate-sev-information = Information
validate-col-severity = Severity
validate-col-location = Location
validate-col-code = Code
validate-col-message = Message
validate-entry-label = entry %n% (%name%)
validate-entry-label-bare = entry %n%
validate-request-failed = The validation request failed (%detail%).
validate-page-heading = Validate
validate-page-lede = Check any resource or Bundle against the FHIR rules and the profiles this server knows, without storing anything.
validate-input = Resource
validate-input-label = Resource or Bundle JSON
validate-input-placeholder = Paste a FHIR resource or Bundle…
validate-profile = Profile (optional)
validate-profile-placeholder = e.g. https://twcore.mohw.gov.tw/ig/twcore/StructureDefinition/Patient-twcore
validate-profile-hint = A StructureDefinition canonical URL; the list offers the profiles stored on this server. The resource’s own meta.profile is always checked too.
validate-profiles-loaded = %count% profiles available

## FHIR package (IG) management (/ui/packages)
nav-packages = IG Packages
packages-title = IG Packages
packages-lede = Install, upgrade and remove FHIR Implementation Guides (npm packages). Profiles and other conformance resources go into this server; terminology goes to the terminology server.
packages-disabled = Package administration is turned off. Set HFS_ADMIN_UI=true on the server to install and remove packages here.
packages-back = Back to IG Packages
packages-tabs-label = Package views
packages-tab-installed = Installed
packages-tab-market = Browse marketplace
packages-tab-upload = Upload .tgz
packages-status-registry = Registry
packages-status-terminology = Terminology server
packages-status-cache = Package cache
packages-registry-public = packages.fhir.org (packages2.fhir.org as fallback)
packages-registry-mirror = Mirror: %urls%
packages-registry-off = Off — uploads and cached packages only
packages-terminology-none = Not configured — terminology is stored in this server
packages-installed-search = Filter installed packages…
packages-col-name = Package
packages-col-version = Version
packages-col-fhir = FHIR
packages-col-installed = Installed
packages-col-resources = Resources
packages-col-dependencies = Dependencies
packages-col-status = Status
packages-installed-empty = No packages are installed yet. Browse the marketplace or upload a .tgz.
packages-installed-count = Packages installed: %count%
packages-state-installed = installed
packages-state-installing = installing
packages-state-failed = failed
packages-state-cancelled = cancelled
packages-explicit = requested
packages-dependency-of = dependency
packages-jobs-heading = Recent jobs
packages-jobs-empty = No package jobs since the server started.
packages-job-plan = plan
packages-job-install = install
packages-job-uninstall = remove
packages-job-queued = queued
packages-job-running = running
packages-job-succeeded = done
packages-job-completed-with-errors = done with errors
packages-job-failed = failed
packages-job-cancelled = stopped
packages-market-search = Search guides by name, title or publisher…
packages-filter-category = Category
packages-filter-country = Country
packages-filter-authority = Publisher
packages-filter-fhir = FHIR version
packages-filter-any = Any
packages-market-count = Guides: %count%
packages-market-unavailable = The marketplace index is not available: %detail%
packages-market-empty = No guides match these filters.
packages-market-more = Show more
packages-latest = Latest %version%
packages-installed-versions = Installed: %versions%
packages-install = Install…
packages-install-by-name = Install by package name
packages-install-name = Package name
packages-install-name-placeholder = e.g. hl7.fhir.us.core
packages-install-version = Version
packages-install-version-latest = Latest
packages-install-version-hint = Versions come from the registry; with the registry off, from the package cache.
packages-registry-results = Other packages in the registry
packages-registry-results-empty = No other registry packages match.
packages-upload-drop = Drop a FHIR package (.tgz) here
packages-upload-browse = or click to choose a file
packages-upload-hint = Works offline: the archive is checked (paths, sizes, manifest) and kept in the server's package cache. With the registry off, upload the dependencies too, or skip them in the plan.
packages-upload-reading = Uploading %name%…
packages-upload-failed = Upload failed: %detail%
packages-wizard-heading = Install %package%
packages-wizard-steps = Install steps
packages-step-tree = Dependencies
packages-step-options = Options
packages-step-progress = Progress
packages-step-result = Result
packages-wizard-planning = Resolving dependencies and reading packages…
packages-tree-lede = The package and everything it needs. Core FHIR definitions are built into this server and never imported.
packages-action-install = install
packages-action-reinstall = reinstall
packages-action-upgrade = upgrade from %from%
packages-action-satisfied = already installed (%by%)
packages-action-embedded = built into the server
packages-action-skipped = skipped
packages-action-missing = not found
packages-skip-dependency = Skip
packages-counts-hfs = %count% resources to this server
packages-counts-hts = %count% to the terminology server
packages-counts-examples = %count% examples skipped
packages-conflict-adopts = %count% already stored, identical (adopted)
packages-conflict-overwrites-unmanaged = %count% overwrite a resource no package owns (kept on removal)
packages-conflict-shared = %count% shared with another package
packages-conflict-replaces-version = %count% replace the installed version
packages-conflict-renamed = %count% get a version-suffixed id
packages-conflict-renamed-clash = %count% share an id with a different resource and get a new id
packages-plan-totals = Packages to install: %packages% · resources to this server: %hfs% · to the terminology server: %hts%
packages-plan-warning = Note: %detail%
packages-plan-failed = The plan could not be made: %detail%
packages-plan-package-error = %package%: %detail%
packages-opt-examples = Include example resources
packages-opt-examples-hint = Examples (package/example/, or marked as examples by the IG) are not imported by default.
packages-opt-dependencies = Install dependencies
packages-opt-mode = When another version is installed
packages-opt-mode-upgrade = Upgrade: the new version replaces it
packages-opt-mode-side = Side by side: keep both (clashing ids get a version suffix)
packages-opt-policy = Dependency versions
packages-opt-policy-newer = An installed newer version satisfies a dependency (recommended)
packages-opt-policy-exact = Install exactly the versions each package names
packages-opt-terminology = Terminology (CodeSystem, ValueSet, ConceptMap)
packages-opt-terminology-auto = The terminology server (this server when none is configured)
packages-opt-terminology-hfs = This server only
packages-opt-terminology-both = Both
packages-replan = Update plan
packages-next = Next
packages-back-step = Back
packages-start-install = Install
packages-cancel = Cancel
packages-stop = Stop
packages-close = Close
packages-progress-packages = Package %done% of %total%
packages-progress-resources = %done% of %total% resources
packages-col-type = Resource type
packages-col-written = Written
packages-col-failed = Failed
packages-col-skipped = Skipped
packages-log = Log
packages-result-ok = Install finished without errors. Packages installed: %count%.
packages-result-errors = Install finished. Packages installed: %count% · errors: %errors%.
packages-result-failed = The install failed: %detail%
packages-result-cancelled = Stopped. What was written so far is recorded against the package; remove it to clean up.
packages-errors-heading = Errors
packages-col-package = Package
packages-col-resource = Resource
packages-col-message = Message
packages-download-report = Download report (JSON)
packages-view-installed = View installed packages
packages-reindex-hint = %count% SearchParameters were installed. Existing data becomes searchable by them after a reindex.
packages-reindex = Start $reindex
packages-reindex-started = Reindex started.
packages-request-failed = Request failed: %detail%
packages-sign-in = Sign in with an administrator account to manage packages.
packages-detail-loading = Loading…
packages-detail-not-installed = %package% is not installed.
packages-detail-meta = Package
packages-meta-title = Title
packages-meta-canonical = Canonical
packages-meta-fhir = FHIR versions
packages-meta-source = Source
packages-meta-sha1 = SHA-1
packages-meta-verified = verified against the registry
packages-meta-installed-at = Installed
packages-meta-requested-by = Requested by
packages-meta-status = Status
packages-detail-dependencies = Dependencies
packages-detail-no-dependencies = No dependencies.
packages-detail-dependents = Required by
packages-detail-no-dependents = No installed package depends on this one.
packages-dep-embedded = built in
packages-dep-skipped = skipped
packages-dep-missing = not installed
packages-detail-resources = Resources
packages-detail-resources-count = Resources: %count%
packages-target-hfs = this server
packages-target-hts = terminology server
packages-res-shared = also in %packages%
packages-res-preexisting = existed before the install (kept on removal)
packages-res-open = Open
packages-res-browse-type = Browse all %type%
packages-col-id = Id
packages-col-url = Canonical
packages-col-where = Stored in
packages-col-note = Note
packages-upgrade = Upgrade…
packages-upgrade-none = This is the newest published version.
packages-upgrade-to = Upgrade to %version%
packages-remove = Remove…
packages-remove-title = Remove %package%
packages-remove-lede = Deletes the resources only this package brought in, unregisters its profiles and removes its terminology from the terminology server. Resources shared with other packages, or that existed before the install, stay.
packages-remove-blocked = It cannot be removed while %packages% depend on it.
packages-remove-confirm = Type %name% to confirm
packages-remove-submit = Remove package
packages-removing = Removing…
packages-removed = Removed: %deleted% resources deleted, %kept% kept.


## Terminology management (HTS_ADMIN_UI): inventory, delete, jobs, archive import

## Administrator sign-in (HFS_ADMIN_PASSWORD / HTS_ADMIN_PASSWORD), shared by
## both consoles: /ui/admin/login and /ui/hts/login.
admin-login-title = Administrator sign-in
admin-login-lede = Installing packages, managing tenants, importing terminology and the other administrative pages need the administrator password.
admin-login-user = User name
admin-login-password = Password
admin-login-submit = Sign in
admin-login-invalid = Wrong user name or password.
admin-login-throttled = Too many failed attempts from your address. Try again in { $minutes } minutes.
admin-login-cross-site = The sign-in form must be sent from this site. Reload the page and try again.
admin-login-locked = Administration is locked: this server has no administrator password. Set { $var } (or { $var }_FILE) and restart it.
admin-user-hint = Administrator

## Navigation between and within the consoles (UI/UX review §4.2): the
## topbar product switcher both consoles share, and the breadcrumb trail.
product-switch-label = Switch console
product-switch-fhir = FHIR Server
product-switch-terminology = Terminology
product-switch-new-tab = Opens in a new tab
breadcrumb-label = Breadcrumb
# The editor's heading before the resource has an id, e.g. "New Patient".
editor-new-title = New { $type }
editor-crumb-new = New
# The history page's heading once an instance is chosen; $subject is "Patient/123".
history-heading-for = History · { $subject }
history-crumb = History
# Tenants page (UI/UX review §3.8): the default tenant cannot be deleted, and
# Delete opens a dialog that states the impact and asks for the id.
tenants-default-tag = Default
tenants-default-hint = The server's default tenant: requests that name no tenant land here, so it cannot be deleted.
tenants-error-delete-default = "{ $id }" is the server's default tenant and cannot be deregistered here.
tenants-delete-title = Deregister tenant "{ $id }"?
tenants-delete-impact = { $count ->
    [one] Tenant "{ $id }" holds one resource.
   *[other] Tenant "{ $id }" holds { $count } resources.
    } Deregistering removes it from the tenant registry; its stored data is kept unless it is purged through the API.
tenants-delete-type-id = Type { $id } to confirm
tenants-delete-submit = Deregister
batch-connection-dropped = The connection closed before the server answered. If your session has expired, sign in again; the bundle may also be larger than this server accepts.

# SQL Library selection and unavailable collection states (#1884).
sql-library-select-title = Select a query or view
sql-library-select-lede = Choose an item from the list or create a new one.
sql-library-unavailable-title = The selection is unavailable
sql-library-unavailable-lede = The available queries and views could not be verified.


# Console redesign: sidebar blocks and route tabs (REDESIGN.md §5)
nav-route-tabs = Pages in this section
rail-show-empty = Show empty
activity-jobs = Jobs
activity-idle = All jobs idle
activity-running = { $count ->
    [one] 1 job running
   *[other] { $count } jobs running
}
activity-detail = Terminology imports running: { $imports } · Sync runs: { $syncs }

## List-and-editor workspace (hm/vd-layout): rail toggle, quick switcher, editor/form split.
ws-rail-collapse = Collapse list
ws-rail-expand = Show list
ws-switch-vd = Switch view definition
ws-switch-lib = Switch to another item
ws-switch-filter = Filter by name
ws-switch-none = No matches
ws-switch-more = Open the full list
ws-pane-label = Editor layout
ws-pane-json = JSON
ws-pane-both = Both
ws-pane-form = Form
ws-split-label = Resize the editor and the form
sb-collapse = Collapse sidebar
sb-expand = Expand sidebar
sb-collapsed = Sidebar collapsed
sb-expanded = Sidebar expanded
