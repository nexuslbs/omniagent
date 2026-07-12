"""Wrapper: run GROUP 8 tests only, skip git check."""
import os, sys
os.environ["SKIP_GIT_CHECK"] = "1"

import tests_toolbox

# From inside the Docker network, omniagent is reachable by container name
tests_toolbox.BASE = "http://omniagent:8080"
tests_toolbox.check_git_clean = lambda: None
tests_toolbox.discard_all_changes = lambda: None

# Only run GROUP 8 tests
tests = [
    tests_toolbox.test_t8_add_remote_new,
    tests_toolbox.test_t8_add_remote_duplicate,
    tests_toolbox.test_t8_remove_bundled_remote_yml_unchanged,
]

passed = 0
failed = 0
for test_fn in tests:
    name = test_fn.__name__
    print(f"  {name} ... ", end="", flush=True)
    try:
        test_fn()
        print("PASS")
        passed += 1
    except Exception as e:
        print(f"FAIL: {e}")
        import traceback
        traceback.print_exc()
        failed += 1

print(f"\n{'='*50}")
print(f"GROUP 8: {passed}/{passed+failed} passed")
sys.exit(0 if failed == 0 else 1)
