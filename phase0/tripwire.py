"""Proves the tests never touch the REAL login in ~/.claude.
record: remember the real login's identity. check: compare.
Fails only if the ACCOUNT changed or the login vanished. A normal token
renewal by your running sessions is reported but is not a failure."""
import json, os, sys
from common import REAL_DIR, config_paths, load, describe

STATE = os.path.join(os.path.dirname(os.path.abspath(__file__)), ".tripwire.json")

def snapshot():
    cred_path, ident_path = config_paths(REAL_DIR)
    d = describe(load(cred_path), load(ident_path).get("oauthAccount"))
    return {"identity": [d["accountUuid"], d["organizationUuid(identity)"], d["organizationUuid(credentials)"]],
            "has_login": d["access token h left"] is not None,
            "refreshToken hash": d["refreshToken hash"],
            "refresh token h left": d["refresh token h left"]}

now = snapshot()
if (sys.argv[1:] or ["check"])[0] == "record":
    with open(STATE, "w") as f:
        json.dump(now, f)
    print("recorded:", now)
    sys.exit(0)
old = load(STATE)
if now["identity"] != old["identity"] or not now["has_login"]:
    print("FAIL: real login changed account or is gone", "\nbefore:", old, "\nnow:   ", now)
    sys.exit(1)
renewed = now["refreshToken hash"] != old["refreshToken hash"]
print("OK: real login is the same account" + (" (tokens renewed by a running session, normal)" if renewed else ", untouched"),
      f'- refresh token {now["refresh token h left"]} h left')
