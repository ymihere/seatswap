"""Phase 0 item 1: refresh a SAVED, INACTIVE test login once and report whether
the login's end date moves. Writes the result as a new vault file.
Usage: refresh.py <vault file>"""
import json, os, sys, time, urllib.request, urllib.error
from common import TEST_DIR, REAL_DIR, VAULT, config_paths, load, hours_left, short_hash

TOKEN_URL = "https://platform.claude.com/v1/oauth/token"
USAGE_URL = "https://api.anthropic.com/api/oauth/usage"
CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"

def http(method, url, headers, body=None):
    # Cloudflare in front of platform.claude.com rejects Python's default
    # User-Agent ("error code: 1010"), so name ourselves plainly.
    headers = {"User-Agent": "claude-switch-phase0/0.1", **headers}
    req = urllib.request.Request(url, method=method, headers=headers,
                                 data=None if body is None else json.dumps(body).encode())
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        raw = e.read().decode(errors="replace")
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, {"raw": raw[:300]}

src = sys.argv[1]
saved = load(src)
old = saved["login"]["claudeAiOauth"]
acct = saved["oauthAccount"]

# Never touch a login that a session may be using.
for d in (TEST_DIR, REAL_DIR):
    live = (load(config_paths(d)[0]).get("claudeAiOauth") or {}).get("refreshToken")
    if live == old["refreshToken"]:
        sys.exit(f"REFUSED: this login is live in {d}; refresh only inactive logins")

print(f"account: {acct['emailAddress']} / {acct['organizationName']}")
print(f"before:  refresh token {hours_left(old['refreshTokenExpiresAt'])} h left "
      f"(ends {time.strftime('%Y-%m-%d %H:%M', time.localtime(old['refreshTokenExpiresAt']/1000))}), "
      f"access token {hours_left(old['expiresAt'])} h left")

t0 = time.time()
status, resp = http("POST", TOKEN_URL, {"Content-Type": "application/json"},
                    {"grant_type": "refresh_token", "refresh_token": old["refreshToken"],
                     "client_id": CLIENT_ID, "scope": " ".join(old["scopes"])})
if status != 200:
    sys.exit(f"refresh failed: HTTP {status} {resp}")

owner = (resp.get("account", {}).get("uuid"), resp.get("organization", {}).get("uuid"))
if owner != (acct["accountUuid"], acct["organizationUuid"]):
    sys.exit(f"REFUSED to save: token belongs to {owner}, slot is {acct['accountUuid'], acct['organizationUuid']}")

rotated = "refresh_token" in resp and resp["refresh_token"] != old["refreshToken"]
new_end_ms = (t0 + resp["refresh_token_expires_in"]) * 1000 if "refresh_token_expires_in" in resp else None
print("response keys:", sorted(resp))
print(f"refresh token rotated: {rotated}")
if new_end_ms is None:
    print("server gave no refresh_token_expires_in")
else:
    moved_h = round((new_end_ms - old["refreshTokenExpiresAt"]) / 3_600_000, 2)
    print(f"server says: refresh token {hours_left(new_end_ms)} h left "
          f"(ends {time.strftime('%Y-%m-%d %H:%M', time.localtime(new_end_ms/1000))}) "
          f"-> end date moved by {moved_h} h")

new = dict(old, accessToken=resp["access_token"], expiresAt=int((t0 + resp["expires_in"]) * 1000))
if rotated:
    new["refreshToken"] = resp["refresh_token"]
if new_end_ms is not None:  # store what the server said; claude-switch would keep min(old,new)
    new["refreshTokenExpiresAt"] = int(new_end_ms)
out_saved = dict(saved, savedAt=time.time(), login=dict(saved["login"], claudeAiOauth=new),
                 refreshedFrom=os.path.basename(src))
out = src.replace(".json", f"-refreshed-{time.strftime('%H%M%S')}.json")
fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "w") as f:
    json.dump(out_saved, f, indent=1)
print("saved", out)

status, usage = http("GET", USAGE_URL, {"Authorization": f"Bearer {new['accessToken']}",
                                        "anthropic-beta": "oauth-2025-04-20"})
print(f"usage with new access token: HTTP {status}",
      {k: usage.get(k) for k in ("five_hour", "seven_day")} if status == 200 else usage)
