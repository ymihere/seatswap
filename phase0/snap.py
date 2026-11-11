"""Save the login currently in the TEST folder into the vault, under a label.
Usage: snap.py <label>      e.g. snap.py AS   (never reads ~/.claude)"""
import sys
from common import save_test_login

out, desc = save_test_login(sys.argv[1])
print("saved", out)
for k, v in desc.items():
    print(f"  {k}: {v}")
