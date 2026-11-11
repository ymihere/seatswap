"""Watch what Claude Code does with its lock folders and login files, optionally
while we hold one of its locks. Only touches ~/cs-test.

  lockwatch.py [--expire] [--hold NAME SECONDS]

  --expire          mark the test login's access token as expired first, so
                    Claude Code has to renew it
  --hold NAME SECS  hold lock folder NAME inside ~/cs-test for SECS seconds
                    (mkdir + fresh mtime every second), e.g. .oauth_refresh.lock,
                    .storage-write.lock, .claude.json.lock
Then runs one `claude -p` against ~/cs-test and prints, in order, every create /
delete / rename of a lock, .credentials.json or .claude.json (inotify)."""
import argparse, ctypes, os, struct, subprocess, threading, time
from common import TEST_DIR, config_paths, load, write_json_atomic, short_hash, hours_left

ap = argparse.ArgumentParser()
ap.add_argument("--expire", action="store_true")
ap.add_argument("--hold", nargs=2, metavar=("NAME", "SECONDS"))
args = ap.parse_args()

IN_CREATE, IN_DELETE, IN_MOVED_TO, IN_ISDIR = 0x100, 0x200, 0x80, 0x40000000
libc = ctypes.CDLL("libc.so.6", use_errno=True)
fd = libc.inotify_init1(0)
watches = {libc.inotify_add_watch(fd, d.encode(), IN_CREATE | IN_DELETE | IN_MOVED_TO): d
           for d in (TEST_DIR, os.path.dirname(TEST_DIR))}
events, t0 = [], time.time()

def reader():
    while True:
        buf, i = os.read(fd, 65536), 0
        while i < len(buf):
            wd, mask, _, ln = struct.unpack_from("iIII", buf, i)
            name = buf[i + 16:i + 16 + ln].rstrip(b"\0").decode()
            i += 16 + ln
            kind = "create" if mask & IN_CREATE else "delete" if mask & IN_DELETE else "rename-in"
            events.append((time.time() - t0, kind, "dir " if mask & IN_ISDIR else "file", os.path.join(watches[wd], name)))

threading.Thread(target=reader, daemon=True).start()
cred_path, _ = config_paths(TEST_DIR)
before = dict(load(cred_path)["claudeAiOauth"])
if args.expire:
    creds = load(cred_path)
    creds["claudeAiOauth"]["expiresAt"] = int(time.time() * 1000) - 60_000
    write_json_atomic(cred_path, creds)

if args.hold:
    lock, secs = f"{TEST_DIR}/{args.hold[0]}", float(args.hold[1])
    os.mkdir(lock)                       # fails if Claude Code holds it right now: good
    def holder():
        end = time.time() + secs
        while time.time() < end:
            os.utime(lock)
            time.sleep(min(1, max(0, end - time.time())))
        os.rmdir(lock)
    threading.Thread(target=holder).start()
    print(f"holding {args.hold[0]} for {secs:.0f} s")

t0 = time.time()
events.clear()
p = subprocess.run(["claude", "-p", "Reply with exactly: OK"], env=dict(os.environ, CLAUDE_CONFIG_DIR=TEST_DIR),
                   cwd="/tmp", capture_output=True, text=True, timeout=170)
took = time.time() - t0
time.sleep(1)
after = load(cred_path)["claudeAiOauth"]
print(f"claude said: {p.stdout.strip()[:40]!r} | exit {p.returncode} after {took:.1f} s")
print("login renewed:", short_hash(before["refreshToken"]) != short_hash(after["refreshToken"]),
      "| end date moved by", round((after["refreshTokenExpiresAt"] - before["refreshTokenExpiresAt"]) / 3_600_000, 2), "h",
      "| access token", hours_left(after["expiresAt"]), "h left")
home, shown, counts = os.path.expanduser("~"), 0, {}
for t, kind, typ, path in events:
    rel = path.replace(home, "~")
    base = os.path.basename(rel)
    if not (base.endswith(".lock") or "lock.owner" in base or base in (".credentials.json", ".claude.json")):
        continue
    counts[(kind, rel)] = counts.get((kind, rel), 0) + 1
    if shown < 16:
        print(f"{t:7.3f}s {kind:9} {typ} {rel}")
        shown += 1
print("totals:", {f"{k} {r}": n for (k, r), n in sorted(counts.items(), key=lambda x: -x[1])})
