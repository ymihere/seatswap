"""Phase 0 item 2: switch the TEST folder (~/cs-test) to a saved login, the way
spec §6.2 describes, by hand. Only ever touches ~/cs-test.
Usage: switch.py <label of current login> <vault file of target>"""
import os, sys
from common import TEST_DIR, LOGIN_KEYS, config_paths, load, describe, write_json_atomic, save_test_login

cur_label, target_file = sys.argv[1], sys.argv[2]
for lock in (f"{TEST_DIR}/.oauth_refresh.lock", f"{TEST_DIR}.lock"):
    if os.path.exists(lock):
        sys.exit(f"REFUSED: Claude Code is renewing a login ({lock} exists); try again")

cred_path, ident_path = config_paths(TEST_DIR)
target = load(target_file)

out, _ = save_test_login(cur_label)              # 1. save the current login first
print("saved current login to", out)

creds = load(cred_path)                          # 2. swap the login keys, keep the rest
for k in LOGIN_KEYS:
    creds.pop(k, None)
creds.update(target["login"])
write_json_atomic(cred_path, creds)

for attempt in range(5):                         # 3. swap oauthAccount, keep the rest
    before = os.stat(ident_path)
    ident = load(ident_path)
    ident["oauthAccount"] = target["oauthAccount"]
    after = os.stat(ident_path)
    if (before.st_mtime_ns, before.st_size) == (after.st_mtime_ns, after.st_size):
        write_json_atomic(ident_path, ident, indent=2)
        break
else:
    sys.exit("FAILED: login switched, identity NOT (.claude.json kept changing)")

d = describe(load(cred_path), load(ident_path).get("oauthAccount"))
print(f"switched test folder to {d['email']} / {d['org']} ({d['plan']})")
