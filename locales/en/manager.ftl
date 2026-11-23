# HealthyMind FHIR Server Manager — manager console catalog (package P5)
# Locale: English (en) — SOURCE LOCALE. zh-Hant must define the same keys
# (checked by crates/ui tests); de and es fall back to English.

## Navigation

mgr-nav-section-manager = Manager
mgr-nav-section-data = Data
mgr-nav-section-configure = Configure
mgr-nav-section-operate = Operate
mgr-nav-section-terminology = Terminology
mgr-nav-section-idp = Authorization servers
mgr-nav-section-server = Server
mgr-nav-section-settings = Settings
mgr-nav-overview = All endpoints
mgr-nav-endpoint-home = Endpoint home
mgr-nav-config = General
mgr-nav-security = Security
mgr-nav-validation = Validation & IGs
mgr-nav-terminology = Terminology service
mgr-nav-sync = Sync
mgr-nav-audit = Audit trail
mgr-nav-history = Config history
mgr-nav-about = About
mgr-switch-endpoint = Switch endpoint
mgr-all-endpoints = All endpoints
mgr-endpoint-count = { $count ->
    [one] { $count } endpoint
   *[other] { $count } endpoints
}
mgr-breadcrumb = Breadcrumb
mgr-skip-to-content = Skip to content
mgr-footer-brand = HealthyMind Tech
mgr-footer-based-on = Based on Helios FHIR Server (MIT License) by Helios Software
mgr-copied = Copied to clipboard
mgr-back = Back
mgr-next = Next
mgr-back-overview = Back to all endpoints

## Overview

mgr-overview-title = FHIR endpoints
mgr-overview-lede = Every FHIR service this server exposes, each with its own base URL, FHIR version, security and validation.
mgr-action-new-endpoint = New endpoint
mgr-action-open = Open
mgr-action-browse-data = Browse data
mgr-action-duplicate = Duplicate
mgr-action-enable = Enable
mgr-action-disable = Disable
mgr-action-delete = Delete endpoint
mgr-action-edit = Edit
mgr-action-view-all = View all
mgr-action-create = Create endpoint
mgr-more-actions = More actions for { $endpoint }
mgr-default-endpoint = Default endpoint (root path)
mgr-copy-base-url = Copy base URL
mgr-copy-of = { $name } (copy)
mgr-stat-resources = Resources
mgr-stat-types = Types
mgr-stat-rpm = Req/min
mgr-stat-last-write = Last write
mgr-sparkline-label = Requests per minute over the last hour
mgr-filter-label = Filter endpoints
mgr-filter-search = Search by name, path or label
mgr-filter-status = Status
mgr-filter-all = All
mgr-filter-apply = Apply
mgr-filter-clear = Clear filters
mgr-showing = { $count ->
    [one] Showing { $count } endpoint
   *[other] Showing { $count } endpoints
}
mgr-empty-title = No endpoints yet
mgr-empty-text = An endpoint is a FHIR service with its own base URL, FHIR version, authentication and validation, stored separately from the others. Create one for an IG sandbox, a partner, or a staging copy.
mgr-nomatch-title = No endpoint matches
mgr-nomatch-text = Try another search term or status filter.
mgr-flash-created = Endpoint { $endpoint } was created.
mgr-flash-status = Endpoint { $endpoint } was updated.
mgr-health-title = Server health
mgr-health-database = Database
mgr-health-terminology = Terminology
mgr-health-instances = Instances
mgr-health-keyring = Secret key ring
mgr-health-console = Console sign-in
mgr-health-ok = Healthy
mgr-health-degraded = Degraded
mgr-health-down = Unreachable
mgr-health-unknown = Not reported
mgr-health-keyring-set = Configured
mgr-health-keyring-unset = Not configured
mgr-console-none = Open (no sign-in)
mgr-console-oidc = Single sign-on (OIDC)
mgr-console-local = Built-in accounts

## Badges

mgr-status-enabled = Enabled
mgr-status-disabled = Disabled
mgr-status-provisioning = Provisioning
mgr-status-deleted = Deleted
mgr-disabled-short = disabled
mgr-auth-off = Auth off
mgr-auth-write-protect = Write-protected
mgr-auth-full = SMART required
mgr-validation-off = Validation off
mgr-validation-log = Validation: log
mgr-validation-enforce = Validation: enforce
mgr-inherited-short = server
mgr-env-managed = Managed by config file
mgr-none = None

## Endpoint home

mgr-kpi-title = Key figures
mgr-kpi-types = { $count ->
    [one] { $count } resource type
   *[other] { $count } resource types
}
mgr-kpi-pending = Counting…
mgr-kpi-export = Export jobs
mgr-kpi-import = Import jobs
mgr-kpi-active = queued or running
mgr-kpi-dashboard = Resources over time
mgr-kpi-dashboard-sub = Open the dashboard
mgr-home-config = Configuration at a glance
mgr-home-types = Top resource types
mgr-home-types-empty = No resources stored yet.
mgr-home-activity = Recent activity
mgr-home-changes = Recent configuration changes
mgr-server-default = Server default
mgr-changed-fields = { $count ->
    [one] { $count } field changed
   *[other] { $count } fields changed
}
mgr-change-create = Created
mgr-change-update = Updated
mgr-change-status = Status changed
mgr-outcome-ok = Success
mgr-outcome-failed = Failed

## Configuration forms

mgr-saved = Changes saved. They apply to new requests right away.
mgr-save = Save changes
mgr-save-bar = Save changes
mgr-review-changes = Review changes
mgr-diff-none = No changes yet.
mgr-diff-field = Field
mgr-diff-before = Before
mgr-diff-after = After
mgr-form-errors = { $count ->
    [one] Please fix { $count } problem below.
   *[other] Please fix { $count } problems below.
}
mgr-conflict = Someone else changed this endpoint while you were editing (now revision { $revision }). Reload to see their change, or overwrite it with yours.
mgr-conflict-reload = Reload
mgr-conflict-overwrite = Overwrite with my changes
mgr-locked-env = This endpoint is declared in HFS_ENDPOINTS_FILE and is read-only here; change the file and restart.
mgr-locked-readonly = Endpoint configuration is read-only in this deployment.
mgr-locked-role = You can view this configuration; changing it needs the administrator role.
mgr-inherit-label = Inherit from server
mgr-own-settings = Own settings
mgr-inherit-auth = Server setting: { $mode }
mgr-inherit-validation = Server setting: { $mode }
mgr-inherit-terminology = Server setting: { $url }
mgr-inherit-cors = Server setting: { $origins }
mgr-inherit-rate = Server setting (anonymous / authenticated per minute): { $limits }

mgr-sec-identity = Identity
mgr-sec-identity-desc = How this endpoint is named in the console.
mgr-sec-address = Address and FHIR version
mgr-sec-address-desc = Where clients reach this endpoint and which FHIR release it speaks.
mgr-sec-console = Console access
mgr-sec-console-desc = Whether a signed-in console user can read or write this endpoint's data through the console.
mgr-field-display-name = Display name
mgr-field-name = Name
mgr-field-name-hint = The name never changes after creation.
mgr-field-name-rules = Lower-case letters, digits and dashes; starts with a letter.
mgr-field-description = Description
mgr-field-labels = Labels
mgr-field-labels-hint = One key=value per line, e.g. env=test. Used for filtering.
mgr-field-path = Path
mgr-field-path-root = Root of the FHIR base URL (the default endpoint)
mgr-field-path-hint = The URL segment after the FHIR base. Leave empty to use the name.
mgr-field-path-same = same as name
mgr-field-path-change-hint = Changing the path changes the base URL: every client must be updated.
mgr-field-fhir-version = FHIR version
mgr-field-fhir-version-hint = Only releases this server was built with are offered.
mgr-field-storage = Storage
mgr-field-storage-tenant = Tenant
mgr-field-storage-hint = Fixed after creation.
mgr-field-tenant = Storage tenant
mgr-field-cors = CORS origins
mgr-field-rate-limit = Rate limits
mgr-field-terminology-server = Terminology server
mgr-field-console-access = Console access to data
mgr-console-access-role-based = By role
mgr-console-access-role-based-desc = Viewers read; editors and administrators also write.
mgr-console-access-read-only = Read-only
mgr-console-access-read-only-desc = Every console role reads; nobody writes through the console.
mgr-console-access-off = Off
mgr-console-access-off-desc = The console session is no credential here; use a token.
mgr-field-legacy-header = Accept X-Tenant-ID (legacy multi-tenancy)
mgr-field-legacy-header-hint = Lets clients pick an unbound tenant on the default endpoint. Tenants bound to a named endpoint are always refused.

mgr-sec-auth = FHIR authentication (SMART on FHIR)
mgr-sec-auth-desc = Which requests to this endpoint need an OAuth token, and which identity provider issues them.
mgr-field-auth-mode = Authentication mode
mgr-auth-mode-off = Off
mgr-auth-mode-off-desc = No credential needed; tokens are ignored.
mgr-auth-mode-write-protect = Write-protect
mgr-auth-mode-write-protect-desc = Anyone may read; writes need a token.
mgr-auth-mode-full = Full SMART
mgr-auth-mode-full-desc = Every request except discovery and metadata needs a token.
mgr-field-issuer = Issuer
mgr-field-issuer-hint = Exact iss of accepted tokens.
mgr-field-jwks = JWKS URL
mgr-field-jwks-hint = Where this server fetches the signing keys (may be an internal address).
mgr-field-audience = Audience
mgr-field-audience-hint = Required aud. Give each endpoint its own, so a token for one is refused by the others.
mgr-field-algorithms = Algorithms
mgr-field-algorithms-hint = Comma-separated; empty means RS256, RS384, ES256, ES384.
mgr-field-clock-skew = Clock skew (seconds)
mgr-field-jwks-max-age = JWKS cache lifetime (seconds)
mgr-field-seconds-default-60 = Empty means 60.
mgr-field-seconds-default-300 = Empty means 300.
mgr-test-auth = Test issuer and JWKS
mgr-test-auth-unavailable = The connection test becomes available with the endpoint admin API.
mgr-field-scopes = Accepted scopes
mgr-scope-patient = patient/ scopes
mgr-scope-user = user/ scopes
mgr-scope-system = system/ scopes
mgr-scope-v1 = SMART v1 scopes (.read, .write)
mgr-scope-v1-hint = v2 scopes (.rs, .cruds) are always accepted.
mgr-field-non-compartment = Patient tokens and shared reference types
mgr-non-compartment-allow = Readable (Medication, Practitioner, Organization…)
mgr-non-compartment-deny = Not reachable
mgr-sec-smart = SMART discovery document
mgr-smart-discover = Fill from the issuer's OpenID configuration
mgr-smart-discover-hint = Fields left empty are discovered from the issuer.
mgr-smart-authorize = Authorization endpoint
mgr-smart-token = Token endpoint
mgr-smart-jwks = Public JWKS URI
mgr-accept-admin = Accept the administrator credential
mgr-accept-admin-hint = The break-glass administrator (HFS_ADMIN_PASSWORD) may act on this endpoint.
mgr-sec-cors = CORS
mgr-sec-cors-desc = Which browser apps on other origins may call this endpoint.
mgr-cors-enabled = Answer CORS requests
mgr-cors-origins = Allowed origins
mgr-cors-origins-hint = One origin per line (https://app.example.org), or * for any.
mgr-cors-credentials = Allow credentials (never with *)
mgr-cors-headers = Extra request headers
mgr-cors-headers-hint = Comma-separated, added to the default list.
mgr-sec-rate = Rate limits
mgr-sec-rate-desc = Requests per minute per client, per server instance.
mgr-rate-anonymous = Anonymous per minute
mgr-rate-authenticated = Authenticated per minute
mgr-rate-burst = Burst (seconds of budget)
mgr-rate-zero-unlimited = 0 means unlimited.
mgr-rate-burst-hint = 0 means the server default (10).

mgr-sec-validation = Write validation
mgr-sec-validation-desc = Whether resources written to this endpoint are checked against their profiles.
mgr-field-validation-mode = Validation mode
mgr-validation-mode-off = Off
mgr-validation-mode-off-desc = Writes are not validated ($validate still works).
mgr-validation-mode-log = Log
mgr-validation-mode-log-desc = Validated; problems are logged and writes accepted.
mgr-validation-mode-enforce = Enforce
mgr-validation-mode-enforce-desc = A write with errors is refused (422).
mgr-field-terminology-check = Terminology checking
mgr-field-terminology-check-hint = How code bindings are checked during validation.
mgr-term-check-off = Off
mgr-term-check-embedded = Embedded value sets only
mgr-term-check-remote = Embedded, then the terminology server
mgr-term-check-remote-only = Terminology server only
mgr-stored-profiles = Use profiles stored in this endpoint
mgr-stored-profiles-hint = StructureDefinitions from installed IG packages and uploads.
mgr-required-profiles = Required profiles
mgr-required-profiles-hint = One "ResourceType canonical-url" per line; every write of that type must conform.
mgr-sec-igs = Implementation Guide packages
mgr-sec-igs-desc = IG packages are installed into this endpoint's own storage; other endpoints are not affected.
mgr-igs-manage = Manage IG packages
mgr-igs-validate = Validate a resource

mgr-sec-terminology = Terminology server
mgr-sec-terminology-desc = The server used for $expand, :in searches, remote validation and IG terminology imports.
mgr-term-server = Server URL
mgr-term-server-hint = Address this server calls (may be internal).
mgr-term-public = Console URL
mgr-term-public-hint = Address a browser can open, for links.
mgr-term-timeout = Timeout (seconds)
mgr-term-timeout-hint = Empty means the server default.
mgr-term-token = Access token
mgr-term-token-hint = Sent on terminology imports.
mgr-sec-hts = Terminology section
mgr-sec-hts-desc = Browse the code systems, value sets and concept maps of the terminology service in this console.
mgr-open-hts = Open Terminology

mgr-secret-set = Set
mgr-secret-unset = Not set
mgr-secret-replace = Replace with
mgr-secret-enter = Value
mgr-secret-hint = Write-only: the stored value is never shown. Leave empty to keep it.
mgr-secret-clear = Clear the stored value

mgr-danger-title = Danger zone
mgr-danger-desc = These actions affect every client of this endpoint.
mgr-danger-disable-desc = Requests answer 503 until it is enabled again. Data and configuration are kept.
mgr-danger-enable-desc = The endpoint starts serving requests again.
mgr-danger-delete-desc = The endpoint stops being routed and its name and path become free. Its data stays in storage until it is purged.
mgr-danger-type-name = Type { $endpoint } to confirm
mgr-danger-confirm-mismatch = The name does not match.
mgr-confirm-disable = Disable endpoint { $endpoint }? Its clients will get 503 until it is enabled again.

mgr-warn-no-audience = Authentication is on but no audience is set: a token issued for another endpoint on the same identity provider would be accepted.
mgr-warn-shared-audience = Another endpoint uses the same issuer and audience: tokens are interchangeable between them.
mgr-warn-http-jwks = The JWKS URL uses plain http; use https unless it is an internal address.
mgr-info-admin-credential = The administrator credential is accepted here (normal for break-glass access).
mgr-info-enforce = Validation is enforced: writes with errors are refused.
mgr-info-cors-any = CORS allows any origin.

## Sync, audit, history

mgr-sync-jobs = Sync jobs touching this endpoint
mgr-sync-new = New sync job
mgr-sync-all = All sync jobs
mgr-sync-none = No sync job reads from or writes to this endpoint.
mgr-sync-empty-title = No sync jobs yet
mgr-sync-empty-text = Sync jobs copy resources one way between endpoints or to and from external FHIR servers, once or continuously.
mgr-sync-unavailable-title = Sync is not available
mgr-sync-unavailable = Sync jobs are not enabled on this server yet.
mgr-sync-col-job = Job
mgr-sync-col-direction = Source → target
mgr-sync-col-trigger = Trigger
mgr-sync-col-last-run = Last run
mgr-sync-col-progress = Progress
mgr-sync-written = { $count } written
mgr-trigger-manual = Manual
mgr-trigger-interval = Interval
mgr-trigger-continuous = Continuous
mgr-trigger-subscription = On notification
mgr-run-queued = Queued
mgr-run-running = Running
mgr-run-succeeded = Succeeded
mgr-run-failed = Failed
mgr-run-cancelled = Cancelled

mgr-audit-latest = Latest { $count } events
mgr-audit-search = Search AuditEvents
mgr-audit-empty = No audit events yet
mgr-audit-empty-text = Reads and writes on this endpoint are recorded here when auditing is enabled.
mgr-audit-unavailable = Audit events could not be read.
mgr-audit-col-time = Time
mgr-audit-col-event = Event
mgr-audit-col-entity = Resource
mgr-audit-col-agent = Who
mgr-audit-col-outcome = Outcome
mgr-load-more = Load more

mgr-history-empty = No configuration changes recorded
mgr-history-empty-text = Every change to this endpoint's configuration is kept here with who made it.

## New endpoint and duplicate

mgr-new-title = New endpoint
mgr-new-lede = Create a FHIR service with its own base URL, data, security and validation.
mgr-new-step-basics = Basics
mgr-new-step-basics-desc = Name the endpoint and choose its address and FHIR version.
mgr-new-step-data = Data
mgr-new-step-data-desc = Start empty, adopt an existing tenant, or copy another endpoint.
mgr-new-step-security = Security
mgr-new-step-security-desc = Who may call this endpoint.
mgr-new-step-validation = Validation
mgr-new-step-validation-desc = How writes are checked, and which terminology server is used.
mgr-new-step-review = Review
mgr-new-step-review-desc = Check what will be created before you create it.
mgr-new-display-hint = Shown in the console, e.g. "TW Core sandbox".
mgr-check-empty = Enter a name.
mgr-check-default = "default" is reserved for the default endpoint.
mgr-check-taken = Already used by endpoint { $endpoint }.
mgr-check-available = Available:
mgr-data-empty = Empty
mgr-data-empty-desc = A new, empty storage tenant named after the endpoint.
mgr-data-adopt = Adopt an existing tenant
mgr-data-adopt-desc = Serve data already stored under a tenant that no endpoint uses.
mgr-data-adopt-tenant = Tenant to adopt
mgr-data-adopt-none = Every tenant is already bound to an endpoint.
mgr-data-duplicate = Copy another endpoint
mgr-data-duplicate-desc = Its configuration, optionally its data and IG packages.
mgr-data-duplicate-from = Copy from
mgr-data-duplicate-what = Data to copy
mgr-dup-data-none = Configuration only
mgr-dup-data-current = Current resources
mgr-dup-data-history = Current resources and their history
mgr-dup-packages = Copy IG packages
mgr-dup-auth = Copy authentication settings
mgr-dup-auth-hint = Change the audience afterwards so tokens are not interchangeable.
mgr-dup-keep-in-sync = Keep in sync with the source
mgr-dup-keep-in-sync-hint = Creates a continuous one-way sync job.
mgr-duplicate-unavailable = Copying endpoints needs the sync package, which is not enabled on this server. Create an empty endpoint instead.
mgr-dup-target = New endpoint
mgr-dup-target-desc = The copy gets its own name, path and storage tenant.
mgr-dup-data = What to copy
mgr-dup-data-desc = Copying data runs as a job; the copy stays in "provisioning" until it finishes.
mgr-isolation-note = Endpoints share one database and are separated by tenant. For data that must be isolated (PHI vs. public sandbox), use a separate deployment.
mgr-own-auth-desc = Choose a mode and identity provider for this endpoint.
mgr-own-validation-desc = Choose a validation mode for this endpoint.
mgr-own-terminology-desc = Use another terminology server.
mgr-new-igs-later = IG packages are installed after creation, from the endpoint's Validation page.
mgr-new-preview = Preview
mgr-new-preview-json = Endpoint definition
mgr-new-preview-duplicate = The source's configuration is copied; the copy is created when you confirm.

## Errors

mgr-error-endpoint-title = Endpoint not found
mgr-error-endpoint-text = There is no endpoint with this name. It may have been deleted or renamed.
mgr-error-forbidden-title = Not allowed
mgr-error-forbidden-text = Your role does not allow this action. Ask an administrator.
mgr-error-unavailable-title = Configuration store unavailable
mgr-error-unavailable-text = Endpoint configuration could not be read or written. Try again in a moment.
mgr-error-details = Technical details

## About

mgr-about-version = Version
mgr-about-default-fhir = Default FHIR version
mgr-about-endpoints = Endpoints
mgr-about-attribution = Attribution
mgr-about-attribution-text = HealthyMind FHIR Server Manager by HealthyMind Tech is based on Helios FHIR Server by Helios Software, used under the MIT License. The original copyright notice is kept in the bundled LICENSE.md.
mgr-about-licenses = Open-source components
mgr-about-component = Component
mgr-about-license = License
mgr-updated-at = Last changed { $at }
mgr-nav-settings-auth = Console sign-in
mgr-nav-settings-users = Users & roles
mgr-nav-account = My account

## Persistent shell (in-place navigation, shell.js)
shell-nav-error = This page could not be loaded.
shell-nav-retry = Retry
shell-nav-offline = The server could not be reached. Check your connection and retry.

## Verify panels (configure-then-verify): the panel chrome. Step messages
## (`verify-{code}`) live in verify.ftl.
verify-run = Verify configuration
verify-run-terminology = Verify terminology server
verify-run-peer = Test connection
verify-running = Verifying… this contacts the server and can take a few seconds.
verify-intro-auth = Runs a full check against the authorization server: address, outbound rules, connection, discovery document, issuer, signing keys, SMART capabilities and audience.
verify-intro-terminology = Checks the terminology server: address, outbound rules, connection, capability statement, a LOINC lookup, a value set expansion and response time.
verify-intro-peer = Checks the other server: address, outbound rules, connection, FHIR version, credentials, resource counts and incremental support.
verify-token-toggle = Also test with a token
verify-token-label = Access token (JWT)
verify-token-hint = Paste a token issued by the authorization server. It is checked exactly as this endpoint would check it, and never stored or logged.
verify-status-pass = Passed
verify-status-warn = Warning
verify-status-fail = Failed
verify-status-skipped = Skipped
verify-summary-fail = { $count ->
    [one] 1 problem found. Fix it and verify again.
   *[other] { $count } problems found. Fix the first one; the others may be consequences.
}
verify-summary-ok-endpoint-auth = All checks passed — tokens from this issuer will be accepted.
verify-summary-ok-terminology = All checks passed — this terminology server answers as expected.
verify-summary-ok-sync-source = All checks passed — the source server can be read.
verify-summary-ok-sync-target = All checks passed — the target server can be written.
verify-summary-warn-endpoint-auth = { $count ->
    [one] Passed with 1 warning — tokens will be accepted, but read the warning.
   *[other] Passed with { $count } warnings — tokens will be accepted, but read the warnings.
}
verify-summary-warn-terminology = { $count ->
    [one] Passed with 1 warning.
   *[other] Passed with { $count } warnings.
}
verify-summary-warn-sync-source = { $count ->
    [one] Passed with 1 warning.
   *[other] Passed with { $count } warnings.
}
verify-summary-warn-sync-target = { $count ->
    [one] Passed with 1 warning.
   *[other] Passed with { $count } warnings.
}
verify-checked-at = Checked at { $time }
verify-stale = settings changed since this check — verify again
verify-how-to-fix = How to fix:
verify-contacted = Contacted
verify-go-to-field = Go to field
verify-use-value = Use { $value }
verify-elapsed = { $ms } ms
verify-unavailable = Verification is not available on this server.
verify-failed-to-run = The check could not run: { $reason }

## Security section (UX-11, UX-23)
mgr-effective-title = While inheriting, this endpoint uses the server's settings:
mgr-effective-unset = not set
mgr-effective-from-discovery = from the issuer's discovery document
mgr-effective-customise = Turn off "Inherit" to customise them for this endpoint; Verify checks these values meanwhile.
mgr-field-audience-recommended = Audience (recommended)
mgr-field-audience-hint-recommended = The aud value a token must carry. Give each endpoint its own, so a token for one is refused by the others.
mgr-field-jwks-hint-optional = Leave empty to use the one the issuer publishes. Set it only when this server must fetch the keys from another (e.g. internal) address.
mgr-sec-advanced = Advanced

## Field errors in the request's language (UX-21)
mgr-err-required-auth = Required while tokens are checked.
mgr-err-url = Enter an absolute http:// or https:// address.
mgr-err-url-credentials = Remove the user name and password from the address.
mgr-err-scopes = Allow at least one kind of scope (patient, user or system).
mgr-err-display-name = Use 1 to 120 characters.
mgr-err-required = Required.
mgr-err-algorithm = { $alg } is not a supported signing algorithm.
mgr-errors-fix-in-step = Fix in step { $step }
mgr-errors-go = Go to field

## Endpoint status outcomes (UX-14)
mgr-flash-deleted = Endpoint { $endpoint } was deleted. Its URL now answers 404; its stored FHIR data stays in tenant { $tenant } until you purge it on the Tenants page.
mgr-flash-disabled = Endpoint { $endpoint } was disabled: its URL answers 503 until you enable it again.
mgr-flash-enabled = Endpoint { $endpoint } was enabled and serves requests again.
verify-block-title = This configuration fails verification:
verify-block-consequence = Saved like this, every request carrying a token would be refused. Fix the setting, or save anyway if you know the authorization server is not reachable from here yet.
verify-block-details = Show all checks
verify-block-save-anyway = Save anyway
verify-block-create-anyway = Create anyway
verify-block-anyway-hint = Recorded in the configuration history like every change.
mgr-copy = Copy
mgr-review-edit = Edit

# Console redesign: sidebar blocks and route tabs (REDESIGN.md §5)
mgr-nav-section-endpoint = Endpoint
mgr-nav-home = Overview
mgr-nav-ep-home = Home
mgr-nav-group-data-tools = Data tools
mgr-nav-group-sql = SQL on FHIR
mgr-nav-group-conformance = Conformance
mgr-nav-group-activity = Activity
mgr-nav-group-settings = Settings
mgr-nav-group-console = Console settings
