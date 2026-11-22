//! Fork migration SQL of the authorization server (design §9).
//!
//! The IdP owns the fork-migration block **hm-0020 … hm-0029** so it never
//! collides with the unification line (hm-0011 …). Migrations are applied by
//! name after the upstream ladder (`FORK_MIGRATIONS` in
//! `helios-persistence`, schema.rs); package I2 appends
//! [`FORK_IDP_CORE`] there and runs [`IDP_CORE_SQL`]. Every statement is
//! idempotent (`IF NOT EXISTS`, `ON CONFLICT DO NOTHING`, `DROP TRIGGER IF
//! EXISTS` before `CREATE TRIGGER`), as fork migrations require.
//!
//! Sign-in throttling reuses `admin_login_failures` (scope
//! `IDP:{realm_id}:user` / `IDP:{realm_id}:ip`); browser SSO sessions get
//! their own table because they carry `auth_time`, `amr` and `acr`.
//! Configuration rows bump the `idp` scope of `hfs_config_epochs` so every
//! instance reloads realms, clients and keys (D-6 of the manager design).

/// Phase 1: realms, keys, clients, users, credentials, sessions, consents,
/// grants, one-time artefacts and refresh token families.
pub const FORK_IDP_CORE: &str = "hm-0020-idp-core";

/// Phase 2 (reserved): PAR request URIs, device codes, DPoP nonces,
/// back-channel logout queue, DCR initial access tokens.
pub const FORK_IDP_PHASE2: &str = "hm-0021-idp-phase2";

/// Phase 3 (reserved): UDAP trust anchors and certifications, WebAuthn.
pub const FORK_IDP_PHASE3: &str = "hm-0022-idp-phase3";

/// SQL of [`FORK_IDP_CORE`]. Requires `hm-0008-endpoints` (for
/// `hfs_config_epochs` and `hfs_bump_config_epoch()`).
pub const IDP_CORE_SQL: &str = r#"
INSERT INTO hfs_config_epochs (scope) VALUES ('idp') ON CONFLICT (scope) DO NOTHING;

CREATE TABLE IF NOT EXISTS hfs_idp_realms (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    spec       JSONB NOT NULL,
    enabled    BOOLEAN NOT NULL DEFAULT TRUE,
    revision   BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_by TEXT,
    updated_by TEXT,
    deleted_at TIMESTAMPTZ
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_hfs_idp_realms_name
    ON hfs_idp_realms (name) WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS hfs_idp_signing_keys (
    realm_id    TEXT NOT NULL REFERENCES hfs_idp_realms (id) ON DELETE CASCADE,
    kid         TEXT NOT NULL,
    alg         TEXT NOT NULL CHECK (alg IN ('RS256','RS384','PS256','PS384','ES256','ES384')),
    status      TEXT NOT NULL CHECK (status IN ('next','active','retiring','retired')),
    public_jwk  JSONB NOT NULL,
    private_key TEXT NOT NULL,          -- hmsec:v1 envelope, AAD idp-realm:{realm}:signing-key:{kid}
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    activate_at TIMESTAMPTZ,
    retire_at   TIMESTAMPTZ,
    expire_at   TIMESTAMPTZ,
    PRIMARY KEY (realm_id, kid)
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_hfs_idp_signing_keys_active
    ON hfs_idp_signing_keys (realm_id, alg) WHERE status = 'active';

CREATE TABLE IF NOT EXISTS hfs_idp_clients (
    realm_id          TEXT NOT NULL REFERENCES hfs_idp_realms (id) ON DELETE CASCADE,
    client_id         TEXT NOT NULL,
    spec              JSONB NOT NULL,
    registration      TEXT NOT NULL DEFAULT 'admin' CHECK (registration IN ('admin','dcr','udap')),
    registration_token_hash TEXT,       -- RFC 7592 registration access token (sha256)
    software_id       TEXT,
    disabled          BOOLEAN NOT NULL DEFAULT FALSE,
    revision          BIGINT NOT NULL DEFAULT 1,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_by        TEXT,
    PRIMARY KEY (realm_id, client_id)
);

CREATE TABLE IF NOT EXISTS hfs_idp_users (
    id               TEXT PRIMARY KEY,
    realm_id         TEXT NOT NULL REFERENCES hfs_idp_realms (id) ON DELETE CASCADE,
    username         TEXT NOT NULL,
    kind             TEXT NOT NULL CHECK (kind IN ('patient','practitioner','related-person','staff')),
    profile          JSONB NOT NULL DEFAULT '{}'::jsonb,
    external_subject TEXT,
    disabled         BOOLEAN NOT NULL DEFAULT FALSE,
    must_change_password BOOLEAN NOT NULL DEFAULT FALSE,
    revision         BIGINT NOT NULL DEFAULT 1,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_login_at    TIMESTAMPTZ
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_hfs_idp_users_username
    ON hfs_idp_users (realm_id, lower(username));
CREATE UNIQUE INDEX IF NOT EXISTS uq_hfs_idp_users_external
    ON hfs_idp_users (realm_id, external_subject) WHERE external_subject IS NOT NULL;

CREATE TABLE IF NOT EXISTS hfs_idp_user_links (
    user_id      TEXT NOT NULL REFERENCES hfs_idp_users (id) ON DELETE CASCADE,
    endpoint     TEXT NOT NULL,
    reference    TEXT NOT NULL,
    is_fhir_user BOOLEAN NOT NULL DEFAULT FALSE,
    accessible_patients TEXT[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (user_id, endpoint, reference)
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_hfs_idp_user_links_fhir_user
    ON hfs_idp_user_links (user_id, endpoint) WHERE is_fhir_user;

CREATE TABLE IF NOT EXISTS hfs_idp_credentials (
    id           TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES hfs_idp_users (id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK (kind IN ('password','totp','webauthn','recovery-code')),
    secret       TEXT NOT NULL,          -- argon2id hash, or hmsec:v1 envelope (TOTP seed)
    meta         JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_hfs_idp_credentials_user ON hfs_idp_credentials (user_id, kind);

CREATE TABLE IF NOT EXISTS hfs_idp_sessions (
    id_hash      TEXT PRIMARY KEY,       -- sha256 of the session cookie value
    realm_id     TEXT NOT NULL REFERENCES hfs_idp_realms (id) ON DELETE CASCADE,
    user_id      TEXT NOT NULL REFERENCES hfs_idp_users (id) ON DELETE CASCADE,
    sid          TEXT NOT NULL,          -- OIDC sid claim (not the cookie)
    auth_time    TIMESTAMPTZ NOT NULL,
    amr          TEXT[] NOT NULL DEFAULT '{}',
    acr          TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at   TIMESTAMPTZ NOT NULL,
    revoked_at   TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_hfs_idp_sessions_user ON hfs_idp_sessions (realm_id, user_id);

CREATE TABLE IF NOT EXISTS hfs_idp_consents (
    realm_id   TEXT NOT NULL REFERENCES hfs_idp_realms (id) ON DELETE CASCADE,
    user_id    TEXT NOT NULL REFERENCES hfs_idp_users (id) ON DELETE CASCADE,
    client_id  TEXT NOT NULL,
    endpoint   TEXT NOT NULL,
    scopes     TEXT[] NOT NULL,
    granted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ,
    PRIMARY KEY (realm_id, user_id, client_id, endpoint)
);

CREATE TABLE IF NOT EXISTS hfs_idp_grants (
    id         TEXT PRIMARY KEY,         -- gid claim of every token of the grant
    realm_id   TEXT NOT NULL REFERENCES hfs_idp_realms (id) ON DELETE CASCADE,
    client_id  TEXT NOT NULL,
    subject    TEXT NOT NULL,
    scopes     TEXT[] NOT NULL,
    audience   TEXT[] NOT NULL,
    context    JSONB NOT NULL DEFAULT '{}'::jsonb,
    fhir_user  TEXT,
    sso_session_id TEXT,                 -- digest of the SSO cookie (online_access)
    auth_time  TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,     -- last access/refresh token expiry
    revoked_at TIMESTAMPTZ,
    revoked_reason TEXT
);
CREATE INDEX IF NOT EXISTS idx_hfs_idp_grants_revoked
    ON hfs_idp_grants (realm_id, revoked_at) WHERE revoked_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_hfs_idp_grants_subject ON hfs_idp_grants (realm_id, subject);

CREATE TABLE IF NOT EXISTS hfs_idp_codes (
    code_hash  TEXT PRIMARY KEY,
    realm_id   TEXT NOT NULL REFERENCES hfs_idp_realms (id) ON DELETE CASCADE,
    grant_id   TEXT NOT NULL,
    payload    JSONB NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
-- A consumed code leaves a tombstone so a replay revokes its grant (RFC 6749 §4.1.2).
CREATE TABLE IF NOT EXISTS hfs_idp_code_tombstones (
    code_hash  TEXT PRIMARY KEY,
    realm_id   TEXT NOT NULL,
    grant_id   TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS hfs_idp_launches (
    handle_hash TEXT PRIMARY KEY,
    realm_id    TEXT NOT NULL REFERENCES hfs_idp_realms (id) ON DELETE CASCADE,
    payload     JSONB NOT NULL,
    expires_at  TIMESTAMPTZ NOT NULL
);

-- Pending authorizations between /authorize and the hosted pages (hmix_ handles).
CREATE TABLE IF NOT EXISTS hfs_idp_interactions (
    id_hash    TEXT PRIMARY KEY,
    realm_id   TEXT NOT NULL REFERENCES hfs_idp_realms (id) ON DELETE CASCADE,
    payload    JSONB NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_hfs_idp_interactions_expiry ON hfs_idp_interactions (expires_at);

CREATE TABLE IF NOT EXISTS hfs_idp_refresh_families (
    family_id          TEXT PRIMARY KEY,
    realm_id           TEXT NOT NULL REFERENCES hfs_idp_realms (id) ON DELETE CASCADE,
    grant_id           TEXT NOT NULL REFERENCES hfs_idp_grants (id) ON DELETE CASCADE,
    client_id          TEXT NOT NULL,
    subject            TEXT NOT NULL,
    current_generation INTEGER NOT NULL DEFAULT 0,
    payload            JSONB NOT NULL,
    sso_session_id     TEXT,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    absolute_expiry    TIMESTAMPTZ NOT NULL,
    revoked_at         TIMESTAMPTZ,
    revoked_reason     TEXT
);
CREATE INDEX IF NOT EXISTS idx_hfs_idp_refresh_families_grant ON hfs_idp_refresh_families (grant_id);
CREATE TABLE IF NOT EXISTS hfs_idp_refresh_tokens (
    token_hash TEXT PRIMARY KEY,
    family_id  TEXT NOT NULL REFERENCES hfs_idp_refresh_families (family_id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,
    UNIQUE (family_id, generation)
);

CREATE TABLE IF NOT EXISTS hfs_idp_jti (
    realm_id   TEXT NOT NULL,
    client_id  TEXT NOT NULL,
    jti        TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (realm_id, client_id, jti)
);
CREATE INDEX IF NOT EXISTS idx_hfs_idp_jti_expiry ON hfs_idp_jti (expires_at);

DROP TRIGGER IF EXISTS trg_hfs_idp_realms_epoch ON hfs_idp_realms;
CREATE TRIGGER trg_hfs_idp_realms_epoch
    AFTER INSERT OR UPDATE OR DELETE ON hfs_idp_realms
    FOR EACH STATEMENT EXECUTE FUNCTION hfs_bump_config_epoch('idp');
DROP TRIGGER IF EXISTS trg_hfs_idp_keys_epoch ON hfs_idp_signing_keys;
CREATE TRIGGER trg_hfs_idp_keys_epoch
    AFTER INSERT OR UPDATE OR DELETE ON hfs_idp_signing_keys
    FOR EACH STATEMENT EXECUTE FUNCTION hfs_bump_config_epoch('idp');
DROP TRIGGER IF EXISTS trg_hfs_idp_clients_epoch ON hfs_idp_clients;
CREATE TRIGGER trg_hfs_idp_clients_epoch
    AFTER INSERT OR UPDATE OR DELETE ON hfs_idp_clients
    FOR EACH STATEMENT EXECUTE FUNCTION hfs_bump_config_epoch('idp');
DROP TRIGGER IF EXISTS trg_hfs_idp_grants_epoch ON hfs_idp_grants;
CREATE TRIGGER trg_hfs_idp_grants_epoch
    AFTER UPDATE OF revoked_at ON hfs_idp_grants
    FOR EACH STATEMENT EXECUTE FUNCTION hfs_bump_config_epoch('idp');
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_sql_is_idempotent_by_construction() {
        for stmt in IDP_CORE_SQL
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let s = stmt
                .lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join(" ");
            let s = s.trim();
            if s.is_empty() {
                continue;
            }
            let ok = s.starts_with("CREATE TABLE IF NOT EXISTS")
                || s.starts_with("CREATE INDEX IF NOT EXISTS")
                || s.starts_with("CREATE UNIQUE INDEX IF NOT EXISTS")
                || s.starts_with("DROP TRIGGER IF EXISTS")
                || s.starts_with("CREATE TRIGGER")
                || (s.starts_with("INSERT") && s.contains("ON CONFLICT"));
            assert!(ok, "non-idempotent statement: {s}");
        }
    }

    #[test]
    fn every_trigger_is_dropped_first() {
        let creates = IDP_CORE_SQL.matches("CREATE TRIGGER").count();
        let drops = IDP_CORE_SQL.matches("DROP TRIGGER IF EXISTS").count();
        assert_eq!(creates, drops);
    }

    #[test]
    fn migration_names_in_reserved_block() {
        for n in [FORK_IDP_CORE, FORK_IDP_PHASE2, FORK_IDP_PHASE3] {
            assert!(n.starts_with("hm-002"), "{n}");
        }
    }
}
