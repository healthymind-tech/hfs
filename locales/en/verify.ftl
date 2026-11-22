# "Configure, then verify" reports (crates/auth/src/verify.rs).
# Every step carries a code: `verify-{code}` is the message, `verify-{code}-fix`
# says how to fix it. Keep keys in step with zh-Hant/verify.ftl.

## Step titles

verify-step-mode = Authentication mode
verify-step-input = Input format
verify-step-outbound = Outbound policy
verify-step-connect = Connection
verify-step-discovery = Discovery document
verify-step-issuer = Issuer
verify-step-jwks-url = JWKS URL
verify-step-jwks = Signing keys (JWKS)
verify-step-algorithms = Algorithms
verify-step-smart = SMART capabilities
verify-step-audience = Audience
verify-step-audience-shared = Audience shared with other endpoints
verify-step-token = Token
verify-step-metadata = Capability statement
verify-step-lookup = Code lookup
verify-step-expand = Value set expansion
verify-step-latency = Response time
verify-step-counts = Resource counts
verify-step-write = Write permission
verify-step-incremental = Incremental sync

## Shared

verify-skipped-after-failure = Not checked: step { $after } failed first.
verify-skipped-after-failure-fix = Fix that step, then verify again.

verify-mode-off = Authentication is off: no credential is needed and tokens are ignored.
verify-token-mode-off = The token was not checked because authentication is off.

verify-input-ok = Every URL is well formed.
verify-input-required = { $field } is required.
verify-input-required-fix = Fill in { $field }.
verify-input-not-url = "{ $value }" is not a web address.
verify-input-not-url-fix = Enter a full URL starting with https://, for example https://idp.example.org/realms/fhir.
verify-input-plain-http = { $field } uses plain http: keys and tokens travel unencrypted.
verify-input-plain-http-fix = Use https if the server offers it.

verify-outbound-trusted = Set by the operator: the outbound policy does not apply.
verify-outbound-ok = The outbound policy allows { $hosts }.
verify-outbound-blocked = The outbound policy does not allow connections to { $host }.
verify-outbound-blocked-fix = Add { $host } to HFS_OUTBOUND_ALLOW and restart the server, or use a public https address.
verify-outbound-plain-http = { $host } is reached over plain http, which is only allowed for hosts in HFS_OUTBOUND_ALLOW.
verify-outbound-plain-http-fix = Use https, or add { $host } to HFS_OUTBOUND_ALLOW and restart the server.
verify-outbound-credentials = The URL contains a user name or password.
verify-outbound-credentials-fix = Remove the user:password@ part; enter credentials in their own fields.

verify-connect-ok = Connected to { $host }.
verify-connect-dns = The host name { $host } does not exist (DNS lookup failed).
verify-connect-dns-fix = Check the spelling of the host name, and that this server can resolve it.
verify-connect-refused = { $host } refused the connection: nothing is listening at that address and port.
verify-connect-refused-fix = Check the port, and that the server is running.
verify-connect-timeout = { $host } did not answer in time.
verify-connect-timeout-fix = Check that the server is up and that no firewall blocks this server.
verify-connect-tls = The secure connection to { $host } failed (TLS: certificate or protocol).
verify-connect-tls-fix = Check the certificate (expired, self-signed, wrong host name) or whether the server speaks https on this port.
verify-connect-failed = Could not reach { $host }.
verify-connect-failed-fix = Check the URL and the network between this server and { $host }.

## Endpoint authentication

verify-discovery-ok = Discovery document found.
verify-discovery-404 = No discovery document at { $url }.
verify-discovery-404-fix = Check the issuer path (Keycloak: https://host/realms/<realm>).
verify-discovery-status = The discovery document URL answered HTTP { $status }.
verify-discovery-status-fix = Check the issuer URL and that the identity provider is healthy.
verify-discovery-html = The URL answered with a web page, not JSON.
verify-discovery-html-fix = The issuer is probably a sign-in page or a proxy. Enter the issuer exactly as the identity provider states it.
verify-discovery-not-json = The answer at { $url } is not a JSON discovery document.
verify-discovery-not-json-fix = Check the issuer path; the identity provider must serve .well-known/openid-configuration below it.

verify-issuer-ok = The identity provider states the same issuer.
verify-issuer-mismatch = The IdP says its issuer is { $stated }; you entered { $entered }. Every token would be refused.
verify-issuer-mismatch-fix = Use { $stated } as the issuer.
verify-issuer-not-stated = The discovery document does not state an issuer.
verify-issuer-not-stated-fix = Check that the URL points at an OpenID Connect identity provider.

verify-jwks-url-ok = The JWKS URL matches the one the issuer publishes.
verify-jwks-url-from-discovery = JWKS URL is empty; the issuer publishes { $discovered }.
verify-jwks-url-from-discovery-fix = Leave it empty, or fill it from discovery.
verify-jwks-url-differs = The JWKS URL is { $configured }, but the issuer publishes { $discovered }.
verify-jwks-url-differs-fix = Use the published JWKS URL unless you know why they differ.
verify-jwks-url-missing = No JWKS URL: the field is empty and the issuer publishes none.
verify-jwks-url-missing-fix = Enter the identity provider's JWKS URL.

verify-jwks-ok = { $count } signing key(s): { $kids } ({ $algs }).
verify-jwks-empty = The JWKS holds no signing key.
verify-jwks-empty-fix = Create or activate a signing key in the identity provider.
verify-jwks-no-usable-key = The JWKS holds { $count } key(s), but none is a signing key with a key id (kid).
verify-jwks-no-usable-key-fix = Publish signing keys with "use": "sig" and a "kid".
verify-jwks-not-json = The JWKS URL did not answer with a JSON key set.
verify-jwks-not-json-fix = Check the JWKS URL; it must return a JSON object with a "keys" list.
verify-jwks-404 = No JWKS at { $url }.
verify-jwks-404-fix = Check the JWKS URL, or leave it empty to use the one the issuer publishes.
verify-jwks-status = The JWKS URL answered HTTP { $status }.
verify-jwks-status-fix = Check the JWKS URL and that the identity provider is healthy.
verify-jwks-unreachable = The JWKS at { $url } could not be fetched ({ $reason }).
verify-jwks-unreachable-fix = Check the JWKS URL host, port and the outbound policy.
verify-jwks-foreign-keys = The keys at { $configured } are not the issuer's: none of their key ids appears at { $discovered }. Tokens would fail signature checks.
verify-jwks-foreign-keys-fix = The JWKS belongs to another realm or identity provider. Use { $discovered }.

verify-algorithms-ok = Tokens signed with { $algorithms } can be verified.
verify-algorithms-unsupported = None of the accepted algorithms ({ $configured }) is supported. Supported: { $supported }.
verify-algorithms-unsupported-fix = Choose algorithms from { $supported }.
verify-algorithms-no-match = The accepted algorithms ({ $configured }) match none of the keys ({ $keys }).
verify-algorithms-no-match-fix = Accept { $keys }, or have the identity provider sign with one of { $configured }.

verify-smart-ok = Authorization and token endpoints with PKCE are advertised (grant types: { $grants }).
verify-smart-no-token = The discovery document has no token endpoint (missing: { $missing }).
verify-smart-no-token-fix = Clients cannot obtain tokens from this issuer. Check the identity provider's configuration.
verify-smart-no-authorize = The discovery document has no authorization endpoint (missing: { $missing }).
verify-smart-no-authorize-fix = Apps cannot sign users in; only backend services (client credentials) will work.
verify-smart-no-pkce = PKCE S256 is not advertised (missing: { $missing }).
verify-smart-no-pkce-fix = Enable PKCE (S256) for clients in the identity provider; SMART apps require it.
verify-smart-no-client-credentials = The client_credentials grant is not advertised (missing: { $missing }).
verify-smart-no-client-credentials-fix = Enable service accounts / client credentials if backend services must connect.

verify-audience-ok = Tokens must be issued for { $audience }.
verify-audience-missing = No audience: any token of this issuer is accepted, including tokens for other services.
verify-audience-missing-fix = Set Audience to a value only this endpoint's tokens carry, and add an audience mapper in the IdP.
verify-audience-shared = Endpoint { $other } has the same issuer and audience: a token for one is accepted by the other.
verify-audience-shared-fix = Give each endpoint its own audience.

verify-token-ok = The token is accepted: subject { $sub }, scopes { $scopes }.
verify-token-audience = The token's audience is { $got }; this endpoint expects { $expected }.
verify-token-audience-fix = Add an audience mapper in the IdP, or change Audience.
verify-token-expired = The token expired at { $expired_at }.
verify-token-expired-fix = Request a new token and try again.
verify-token-signature = The token's signature does not match the signing keys.
verify-token-signature-fix = The token was probably issued by another realm or identity provider. Check the issuer and JWKS URL.
verify-token-issuer = The token's issuer is { $got }; this endpoint expects { $expected }.
verify-token-issuer-fix = Request the token from { $expected }, or correct the issuer.
verify-token-algorithm = The token is signed with { $alg }, which this endpoint does not accept.
verify-token-algorithm-fix = Add { $alg } to the accepted algorithms.
verify-token-malformed = This is not a JWT.
verify-token-malformed-fix = Paste the access token itself (three parts separated by dots), without quotes.
verify-token-not-yet-valid = The token is not valid yet.
verify-token-not-yet-valid-fix = Check the clocks of this server and the identity provider.
verify-token-missing-claim = The token lacks the required claim { $claim }.
verify-token-missing-claim-fix = Configure the identity provider to include { $claim }.
verify-token-refused = The token is refused.
verify-token-refused-fix = Check the credentials and the token's details.

## Terminology server

verify-metadata-ok = { $software } { $version } answers (FHIR { $fhir }).
verify-metadata-404 = No capability statement at { $url }.
verify-metadata-404-fix = This is probably not the FHIR base URL. Terminology servers usually live below a path such as /fhir or /tx.
verify-metadata-unauthorized = The server refused /metadata (HTTP { $status }).
verify-metadata-unauthorized-fix = Enter a token the server accepts.
verify-metadata-status = /metadata answered HTTP { $status }.
verify-metadata-status-fix = Check the URL and that the server is healthy.
verify-metadata-not-fhir = The URL did not answer with a FHIR capability statement.
verify-metadata-not-fhir-fix = Check that the URL is the FHIR base of the server.
verify-token-accepted = The server accepts the credentials.
verify-token-not-checkable = The server answers without credentials too, so the token could not be checked.
verify-lookup-ok = LOINC 8867-4 resolves to "{ $display }".
verify-lookup-missing-system = The server does not know { $system }.
verify-lookup-missing-system-fix = Import the terminology (for example LOINC) into the terminology server.
verify-lookup-failed = The code lookup failed.
verify-lookup-failed-fix = Check the server log; $lookup must work for validation.
verify-expand-ok = Administrative gender expands to { $count } code(s).
verify-expand-missing = The server cannot expand { $valueset }.
verify-expand-missing-fix = Load the FHIR core value sets into the terminology server.
verify-expand-failed = The value set expansion failed.
verify-expand-failed-fix = Check the server log; $expand must work for validation.
verify-latency-ok = The slowest answer took { $ms } ms (timeout { $timeout_ms } ms).
verify-latency-high = The slowest answer took { $ms } ms, more than half the { $timeout_ms } ms timeout.
verify-latency-high-fix = Raise the timeout, or use a terminology server closer to this one.

## Sync peers

verify-metadata-version-mismatch = The server speaks FHIR { $got }; this job expects { $expected }.
verify-metadata-version-mismatch-fix = Choose a server with the same FHIR version, or change the job's FHIR version.
verify-token-issued = The token endpoint issued an access token.
verify-token-invalid-client = The token endpoint refused the client.
verify-token-invalid-client-fix = Check the client id and client secret.
verify-token-invalid-scope = The token endpoint refused the requested scope.
verify-token-invalid-scope-fix = Request only scopes the client is allowed, for example system/*.rs.
verify-token-endpoint-404 = No token endpoint at { $url }.
verify-token-endpoint-404-fix = Check the token URL.
verify-token-endpoint-unreachable = The token endpoint could not be reached ({ $reason }).
verify-token-endpoint-unreachable-fix = Check the token URL host and port, and the outbound policy.
verify-token-bad-response = The token endpoint answered HTTP { $status } without an access token.
verify-token-bad-response-fix = Check that the token URL is the OAuth token endpoint.
verify-token-error = The token endpoint answered { $error }.
verify-token-error-fix = Check the client settings in the identity provider.
verify-token-required = The server requires authentication.
verify-token-required-fix = Choose an authentication type and enter credentials.
verify-token-not-needed = The server answers without credentials.
verify-counts-ok = { $summary }
verify-counts-no-types = No resource types selected: every type is copied, so none was counted.
verify-counts-unknown-type = The server does not offer { $type }.
verify-counts-unknown-type-fix = Remove { $type } from the resource types, or check the server's FHIR version.
verify-counts-refused = The server refused the search (HTTP { $status }).
verify-counts-refused-fix = Check that the credentials allow reading these resource types.
verify-write-ok = The server advertises create and update for the selected types.
verify-write-not-advertised = The server does not advertise create or update for { $types }.
verify-write-not-advertised-fix = Writes to these types may be refused. Check the server's capability statement and the client's permissions.
verify-incremental-ok = The server supports _lastUpdated or history: later runs read only changes.
verify-incremental-unsupported = The server advertises neither _lastUpdated nor history for { $types }.
verify-incremental-unsupported-fix = Each run may have to read everything; choose a longer interval.
