"""Show which account/org each account-tied cache in the TEST folder belongs to."""
import base64, json, os, time
from common import TEST_DIR, load
tag = lambda u: u[:8] if u else u   # first 8 characters of an account or org id
ts = lambda p: time.strftime("%H:%M:%S", time.localtime(os.path.getmtime(p)))
d = load(f"{TEST_DIR}/.claude.json")
print("identity:", tag(d["oauthAccount"]["accountUuid"]), "| .claude.json written", ts(f"{TEST_DIR}/.claude.json"))
for k in ("cachedArtifactRoster", "githubWebConnectionStatusCache", "unusedLimitResetStatusCache"):
    v = d.get(k) or {}
    print(f"  {k}: account {tag(v.get('accountUuid'))}")
print("  metricsStatusCacheByPrincipal:", [tag(k.split(':')[1][:8]) for k in d.get("metricsStatusCacheByPrincipal") or {}])
print("  clientDataCacheSlots:", [(tag(v.get("org")), time.strftime("%H:%M:%S", time.localtime(v["at"] / 1000))) for v in (d.get("clientDataCacheSlots") or {}).values()])
print("  additionalModelOptionsCache:", json.dumps(d.get("additionalModelOptionsCache"))[:150])
for f in ("policy-limits.json", "remote-settings.json"):
    sig = f"{TEST_DIR}/{f}.signature.json"
    if not os.path.exists(sig):
        print(f"  {f}: no signature file"); continue
    p = load(sig)["jws"].split(".")[1]
    c = json.loads(base64.urlsafe_b64decode(p + "=" * (-len(p) % 4)))
    print(f"  {f}: org {tag(c.get('aud'))} account {tag(c.get('sub'))} plan {c.get('plan')} signed {time.strftime('%H:%M:%S', time.localtime(c['iat']))} file written {ts(sig)}")
