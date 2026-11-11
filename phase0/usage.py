"""Read usage for saved logins (read-only, no refresh). Usage: usage.py <vault file>..."""
import json, sys, urllib.request
from common import load, hours_left

def usage(access_token):
    req = urllib.request.Request("https://api.anthropic.com/api/oauth/usage", headers={
        "Authorization": f"Bearer {access_token}", "anthropic-beta": "oauth-2025-04-20",
        "User-Agent": "claude-switch-phase0/0.1"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)

if __name__ == "__main__":
    for path in sys.argv[1:]:
        s = load(path)
        o = s["login"]["claudeAiOauth"]
        d = usage(o["accessToken"])
        lim = [(l["kind"], l["percent"], l["severity"], l["is_active"]) for l in d.get("limits") or []]
        print(f'{s["oauthAccount"]["emailAddress"]}: 5h={(d.get("five_hour") or {}).get("utilization")} '
              f'week={(d.get("seven_day") or {}).get("utilization")} limits={lim} (access token {hours_left(o["expiresAt"])} h left)')
