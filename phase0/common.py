"""Shared helpers for the Phase 0 scripts. Never prints token text."""
import hashlib, json, os, time

HOME = os.path.expanduser("~")
REAL_DIR = f"{HOME}/.claude"
REAL_JSON = f"{HOME}/.claude.json"
TEST_DIR = f"{HOME}/cs-test"          # CLAUDE_CONFIG_DIR for every test session
VAULT = f"{HOME}/cs-test-vault"       # saved test logins, 0700 folder, 0600 files

# Keys of .credentials.json that Claude Code 2.1.322 deletes on logout
# (function that strips the login before writing). They make up "the login".
LOGIN_KEYS = ["claudeAiOauth", "organizationUuid", "trustedDeviceToken",
              "trustedDeviceEnrolledAfterRefusalAtMs", "enterpriseGateway",
              "designOauth", "cloudSessionsOauth"]

def load(path):
    with open(path) as f:
        return json.load(f)

def config_paths(config_dir):
    """(credentials file, identity file) for a config folder."""
    if config_dir == REAL_DIR:
        return f"{REAL_DIR}/.credentials.json", REAL_JSON
    return f"{config_dir}/.credentials.json", f"{config_dir}/.claude.json"

def short_hash(obj):
    return hashlib.sha256(json.dumps(obj, sort_keys=True).encode()).hexdigest()[:12]

def hours_left(ms):
    return None if ms is None else round((ms - time.time() * 1000) / 3_600_000, 1)

def describe(creds, account):
    """Non-secret summary of a login."""
    o = creds.get("claudeAiOauth") or {}
    return {
        "email": (account or {}).get("emailAddress"),
        "org": (account or {}).get("organizationName"),
        "accountUuid": (account or {}).get("accountUuid"),
        "organizationUuid(identity)": (account or {}).get("organizationUuid"),
        "organizationUuid(credentials)": creds.get("organizationUuid"),
        "login keys present": [k for k in LOGIN_KEYS if k in creds],
        "other keys": [k for k in creds if k not in LOGIN_KEYS],
        "plan": f'{o.get("subscriptionType")} / {o.get("rateLimitTier")}',
        "access token h left": hours_left(o.get("expiresAt")),
        "refresh token h left": hours_left(o.get("refreshTokenExpiresAt")),
        "refreshToken hash": short_hash(o.get("refreshToken")),
    }

def write_json_atomic(path, obj, indent=None):
    """Temp file in the same folder, fsync, rename; keeps the file mode."""
    mode = os.stat(path).st_mode & 0o777 if os.path.exists(path) else 0o600
    tmp = f"{path}.cs-tmp-{os.getpid()}"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    with os.fdopen(fd, "w") as f:
        json.dump(obj, f, indent=indent)
        f.flush()
        os.fsync(f.fileno())
    os.rename(tmp, path)

def save_test_login(label):
    """Copy the TEST folder's current login + identity into a new vault file."""
    cred_path, ident_path = config_paths(TEST_DIR)
    creds, account = load(cred_path), load(ident_path).get("oauthAccount")
    if not (creds.get("claudeAiOauth") or {}).get("refreshToken"):
        raise SystemExit(f"no login in {cred_path}")
    os.makedirs(VAULT, mode=0o700, exist_ok=True)
    out = f"{VAULT}/{label}-{time.strftime('%Y%m%d-%H%M%S')}.json"
    fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump({"savedAt": time.time(), "login": {k: creds[k] for k in LOGIN_KEYS if k in creds},
                   "oauthAccount": account}, f, indent=1)
    return out, describe(creds, account)
