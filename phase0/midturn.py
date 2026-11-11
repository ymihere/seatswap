"""Phase 0 item 2, mid-turn switch: wait until the test session receives a
prompt containing MIDTURN-TEST, wait <delay> s more, then switch the test
folder to <target vault file>. Usage: midturn.py <cur label> <target> [delay]"""
import glob, os, subprocess, sys, time
from common import TEST_DIR

cur, target = sys.argv[1], sys.argv[2]
delay = float(sys.argv[3]) if len(sys.argv) > 3 else 25
here = os.path.dirname(os.path.abspath(__file__))

def marker_seen(start):
    for f in glob.glob(f"{TEST_DIR}/projects/*/*.jsonl"):
        if os.path.getmtime(f) < start:
            continue
        with open(f) as fh:
            if any('"type":"user"' in l and "MIDTURN-TEST" in l for l in fh):
                return True
    return False

start = time.time()
print(time.strftime("%H:%M:%S"), "waiting for MIDTURN-TEST prompt", flush=True)
while not marker_seen(start):
    time.sleep(1)
print(time.strftime("%H:%M:%S"), f"prompt seen, switching in {delay} s", flush=True)
time.sleep(delay)
subprocess.run([sys.executable, f"{here}/switch.py", cur, target], check=True)
print(time.strftime("%H:%M:%S"), "switched", flush=True)
