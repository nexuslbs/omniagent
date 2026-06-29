#!/usr/bin/env python3
import os, json, urllib.request, urllib.error, sys
MM_URL = "http://mattermost:8065"
TOKEN_VAL = os.environ.get("MATTERMOST_ACCESS_TOKEN", "")
if not TOKEN_VAL:
    print("No MATTERMOST_ACCESS_TOKEN", file=sys.stderr)
    sys.exit(1)
print("Token: %d chars" % len(TOKEN_VAL), flush=True)

def api(method, path, data=None):
    hdrs = {"Authorization": "Bearer " + TOKEN_VAL}
    body = json.dumps(data).encode() if data else None
    if body:
        hdrs["Content-Type"] = "application/json"
    req = urllib.request.Request(MM_URL + path, data=body, headers=hdrs, method=method)
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return resp.status, json.loads(resp.read())
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read())

s, teams = api("GET", "/api/v4/teams")
print("Teams: s=%d count=%d" % (s, len(teams)), flush=True)
team_id = teams[0]["id"]
s, channels = api("GET", "/api/v4/teams/" + team_id + "/channels")
cid = None
for ch in channels:
    if ch["name"] == "test":
        cid = ch["id"]
        break
if not cid:
    s, ch = api("POST", "/api/v4/channels", {"team_id": team_id, "name": "test", "display_name": "Test", "type": "O"})
    cid = ch["id"]
print("Channel: " + str(cid), flush=True)

s, users = api("POST", "/api/v4/users/usernames", ["testuser"])
if isinstance(users, list) and len(users) > 0:
    uid = users[0]["id"]
else:
    s, u = api("POST", "/api/v4/users", {"username": "testuser", "password": "TestPassword123!", "email": "testuser@test.local"})
    uid = u["id"]
    api("POST", "/api/v4/teams/" + team_id + "/members", {"team_id": team_id, "user_id": uid})
print("User: " + str(uid), flush=True)

login_data = json.dumps({"login_id": "testuser", "password": "TestPassword123!"}).encode()
login_req = urllib.request.Request(MM_URL + "/api/v4/users/login", data=login_data,
    headers={"Content-Type": "application/json"}, method="POST")
with urllib.request.urlopen(login_req, timeout=10) as resp:
    tok = resp.headers.get("Token", "")
    post_data = json.dumps({"channel_id": cid, "message": "Hello from testuser!"}).encode()
    post_req = urllib.request.Request(MM_URL + "/api/v4/posts", data=post_data,
        headers={"Authorization": "Bearer " + tok, "Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(post_req, timeout=10) as pr:
        r = json.loads(pr.read())
        print("POST " + str(pr.status), flush=True)
        print("SUCCESS", flush=True)
        print(json.dumps(r, indent=2))
