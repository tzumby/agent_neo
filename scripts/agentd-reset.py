#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["boto3==1.43.123"]
# ///
"""Wipes a desktop's agent and browser, for testing from a clean slate.

  scripts/agentd-reset.py i-0123456789abcdef0 [--files] [--keep-copy] [--dry-run]
  scripts/agentd-reset.py i-0123456789abcdef0 --restore

Over SSM, with agentd and Chrome stopped, it deletes:
- agentd's database: the chat, every task and scheduled run, memories,
  schedules and settings (the model and tidy-up go back to their defaults),
  and the copies of it made before upgrades (agentd.db.pre-*);
- the screenshots the agent sent, its saved keys, and where its updates go
  (the Slack and Teams channels, with their bot tokens and webhook URLs);
- Chrome's whole profile: cookies and every sign-in, site data (local
  storage, IndexedDB), history, tabs and cache; and the Downloads folder.
Then Chrome and agentd start again, empty. --files also empties the shell
user's work folder (/home/worker/work).

Kept: Chrome's policy, push notifications (registered at the relay; the app
registers again when it connects), agentd's config, the desktop.

--keep-copy moves all of it to /var/lib/agentd-reset/<time> (root only)
instead of deleting it; --restore puts the newest copy back, replacing what
is there now. --dry-run only shows what would go.

It shows what it will delete and asks you to type the desktop's name, and it
refuses while a task runs. Only counts leave the desktop (operator-blind):
never messages, memories, sites or the names of saved keys.

Uses the AWS profile and region from the environment (the Makefile sets both;
on its own: your default profile, ca-central-1).
"""

import argparse
import json
import os
import sys
import time

import boto3

# Runs on the desktop as root: `python3 - <action> [--files]`. Prints one JSON line.
REMOTE = r"""
import glob, json, os, pwd, re, shutil, sqlite3, subprocess, sys, time
STATE = "/var/lib/agentd"
WORK = "/home/worker/work"
ARCHIVE = "/var/lib/agentd-reset"
# What agentd keeps (agent.rs: Agent::new; main.rs). Anything else is left alone.
KNOWN = ["agentd.db", "agentd.db-wal", "agentd.db-shm", "images", "secrets", "destinations"]
action, files = sys.argv[1], "--files" in sys.argv

def unit(prop, name="chrome"):
    return subprocess.run(
        ["systemctl", "show", name, "-p", prop, "--value"], capture_output=True, text=True
    ).stdout.strip()

def find_chrome():
    # Its user and profile, from its unit (roles/desktop: chrome.service).
    user = unit("User")
    found = re.search(r"--user-data-dir=([^ ;]+)", unit("ExecStart"))
    if not user or not found:
        return None
    try:
        home = pwd.getpwnam(user).pw_dir
    except KeyError:
        return None
    return {"user": user, "profile": found.group(1), "home": home}

CHROME = find_chrome()

def status():
    out = subprocess.run(
        ["curl", "-s", "--max-time", "5", "--unix-socket", "/run/agentd/agentd.sock", "http://agentd/api/v1/status"],
        capture_output=True, text=True,
    ).stdout
    try:
        return json.loads(out)
    except ValueError:
        return None

def entries(path):
    return len(os.listdir(path)) if os.path.isdir(path) else 0

def files_under(path):
    return sum(len(f) for _, _, f in os.walk(path)) if os.path.isdir(path) else 0

def size_mb(path):
    total = 0
    for root, _, names in os.walk(path):
        for name in names:
            try:
                total += os.lstat(os.path.join(root, name)).st_size
            except OSError:
                pass
    return round(total / 1e6)

def open_db(db):
    # Read-only needs the writer's -shm file (WAL), there while it runs; a
    # stopped agentd, or Chrome's exclusive lock: read the file as it is.
    for uri in (f"file:{db}?mode=ro", f"file:{db}?immutable=1"):
        try:
            conn = sqlite3.connect(uri, uri=True)
            conn.execute("SELECT 1 FROM sqlite_master LIMIT 1")
            return conn
        except sqlite3.Error:
            pass
    return None

def count(db, sql):
    conn = open_db(db) if os.path.exists(db) else None
    if not conn:
        return None
    try:
        return conn.execute(sql).fetchone()[0]
    except sqlite3.Error:
        return None
    finally:
        conn.close()

def counts():
    db = os.path.join(STATE, "agentd.db")
    c = {
        "database": os.path.exists(db),
        "messages": count(db, "SELECT COUNT(*) FROM history"),
        "tasks": count(db, "SELECT COUNT(*) FROM runs WHERE kind = 'task'"),
        "memories": count(db, "SELECT COUNT(*) FROM memory WHERE status = 'active'"),
        "schedules": count(db, "SELECT COUNT(*) FROM schedules"),
        "db_copies": len(glob.glob(os.path.join(STATE, "agentd.db.pre-*"))),
        "images": entries(os.path.join(STATE, "images")),
        "saved_keys": entries(os.path.join(STATE, "secrets")),
        "destinations": entries(os.path.join(STATE, "destinations")),
        "work_files": files_under(WORK),
        "unknown": sorted(
            name for name in (os.listdir(STATE) if os.path.isdir(STATE) else [])
            if name not in KNOWN and not name.startswith("agentd.db.pre-")
        ),
        "copies": sorted(os.listdir(ARCHIVE)) if os.path.isdir(ARCHIVE) else [],
        "chrome": CHROME is not None,
    }
    if CHROME:
        profile = CHROME["profile"]
        c["chrome_user"] = CHROME["user"]
        c["profile_mb"] = size_mb(profile) if os.path.isdir(profile) else 0
        c["cookies"] = next(
            (n for n in (count(f"{profile}/Default/{name}", "SELECT COUNT(*) FROM cookies")
                         for name in ("Network/Cookies", "Cookies")) if n is not None),
            None,
        )
        c["downloads"] = files_under(os.path.join(CHROME["home"], "Downloads"))
    return c

def state_paths():
    paths = [os.path.join(STATE, name) for name in KNOWN]
    paths += glob.glob(os.path.join(STATE, "agentd.db.pre-*"))
    return [p for p in paths if os.path.lexists(p)]

def browser_paths():
    if not CHROME:
        return []
    home = CHROME["home"]
    paths = [CHROME["profile"], os.path.join(home, ".cache", "google-chrome"), os.path.join(home, ".config", "google-chrome")]
    downloads = os.path.join(home, "Downloads")
    if os.path.isdir(downloads):
        paths += [os.path.join(downloads, name) for name in os.listdir(downloads)]
    return [p for p in paths if os.path.lexists(p)]

def work_paths(wanted):
    return [os.path.join(WORK, name) for name in os.listdir(WORK)] if wanted and os.path.isdir(WORK) else []

def remove(path):
    if os.path.isdir(path) and not os.path.islink(path):
        shutil.rmtree(path)
    else:
        os.remove(path)

def chown_tree(path, uid, gid):
    # Only if a move between filesystems (a copy, as root) changed the owner.
    if os.lstat(path).st_uid == uid and os.lstat(path).st_gid == gid:
        return
    os.lchown(path, uid, gid)
    for root, dirs, names in os.walk(path):
        for name in dirs + names:
            os.lchown(os.path.join(root, name), uid, gid)

def wait_until_up():
    for _ in range(30):
        st = status()
        if st:
            return st
        time.sleep(1)
    return None

def working(st):
    # The chat's task, or a scheduled run beside it (agentd 0.15.10).
    return bool(st and (st.get("busy") or (st.get("background") or {}).get("running")))

out = {"before": counts(), "status": status()}
if action != "survey":
    if working(out["status"]):
        out["refused"] = "a task is running"
    elif action == "restore" and not out["before"]["copies"]:
        out["refused"] = "there is no copy to restore"
    else:
        subprocess.run(["systemctl", "stop", "agentd"], check=True)
        if CHROME:
            subprocess.run(["systemctl", "stop", "chrome"], check=True)
        try:
            if action == "restore":
                source = os.path.join(ARCHIVE, out["before"]["copies"][-1])
                with open(os.path.join(source, "manifest.json")) as f:
                    manifest = json.load(f)
                had_work = any(original.startswith(WORK + "/") for _, original, _, _ in manifest)
                for path in state_paths() + browser_paths() + work_paths(had_work):
                    remove(path)
                for name, original, uid, gid in manifest:
                    if os.path.lexists(original):
                        remove(original)
                    os.makedirs(os.path.dirname(original), exist_ok=True)
                    shutil.move(os.path.join(source, name), original)
                    chown_tree(original, uid, gid)
                shutil.rmtree(source)
                out["restored"] = os.path.basename(source)
            elif action == "keep-copy":
                os.makedirs(ARCHIVE, mode=0o700, exist_ok=True)
                os.chmod(ARCHIVE, 0o700)
                dest = os.path.join(ARCHIVE, time.strftime("%Y%m%dT%H%M%S"))
                os.makedirs(dest, mode=0o700)
                manifest = []
                for path in state_paths() + browser_paths() + work_paths(files):
                    name = f"{len(manifest):03d}-{os.path.basename(path)}"
                    st = os.lstat(path)
                    shutil.move(path, os.path.join(dest, name))
                    manifest.append([name, path, st.st_uid, st.st_gid])
                    # Written after each move: whatever happens, the copy says where things go back.
                    with open(os.path.join(dest, "manifest.json"), "w") as f:
                        json.dump(manifest, f)
                out["copy"] = dest
            else:
                for path in state_paths() + browser_paths() + work_paths(files):
                    remove(path)
        finally:
            if CHROME:
                subprocess.run(["systemctl", "start", "chrome"], check=False)
            subprocess.run(["systemctl", "start", "agentd"], check=False)
        out["status"] = wait_until_up()
        out["chrome_running"] = bool(CHROME) and unit("ActiveState") == "active"
        out["after"] = counts()
print("RESET_JSON:" + json.dumps(out))
"""


def region() -> str:
    # boto3 doesn't read AWS_REGION on its own.
    return os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION") or "ca-central-1"


def desktop_name(instance: str) -> str:
    """The desktop's name (its User tag; Name, or the id, without one)."""
    reservations = boto3.client("ec2", region_name=region()).describe_instances(InstanceIds=[instance])["Reservations"]
    tags = {t["Key"]: t["Value"] for t in reservations[0]["Instances"][0].get("Tags", [])}
    return tags.get("User") or tags.get("Name") or instance


def run_remote(instance: str, action: str, files: bool) -> dict:
    ssm = boto3.client("ssm", region_name=region())
    script = f"python3 - {action}{' --files' if files else ''} <<'PY'\n{REMOTE}\nPY"
    command = ssm.send_command(
        InstanceIds=[instance],
        DocumentName="AWS-RunShellScript",
        Parameters={"commands": [script]},
        Comment=f"agentd reset: {action}",
    )["Command"]["CommandId"]
    for _ in range(90):
        time.sleep(2)
        try:
            result = ssm.get_command_invocation(CommandId=command, InstanceId=instance)
        except ssm.exceptions.InvocationDoesNotExist:
            continue
        if result["Status"] in ("Pending", "InProgress", "Delayed"):
            continue
        for line in result["StandardOutputContent"].splitlines():
            if line.startswith("RESET_JSON:"):
                return json.loads(line.removeprefix("RESET_JSON:"))
        sys.exit(f"No answer from the desktop ({result['Status']}): {result['StandardErrorContent'][:500]}")
    sys.exit("Timed out waiting for the desktop. Check it: make agentd-logs, make viewer")


def count(value: int | None, one: str, many: str) -> str:
    return f"? {many}" if value is None else f"{value:,} {one if value == 1 else many}"


def describe(c: dict, files: bool) -> list[str]:
    """What's there, in counts."""
    lines = []
    if c["database"]:
        lines.append(
            f"agentd's database: {count(c['messages'], 'chat message', 'chat messages')}, "
            f"{count(c['tasks'], 'task', 'tasks')}, {count(c['memories'], 'memory', 'memories')}, "
            f"{count(c['schedules'], 'schedule', 'schedules')}, and the settings"
        )
    if c["db_copies"]:
        lines.append(count(c["db_copies"], "database copy", "database copies") + " from upgrades")
    if c["images"]:
        lines.append(count(c["images"], "screenshot", "screenshots"))
    if c["saved_keys"]:
        lines.append(count(c["saved_keys"], "saved key", "saved keys"))
    if c.get("destinations"):
        lines.append(count(c["destinations"], "Slack or Teams channel", "Slack or Teams channels") + " for updates, with their tokens")
    if c["chrome"] and (c["profile_mb"] or c["cookies"]):
        lines.append(
            f"Chrome's profile ({c['profile_mb']:,} MB): {count(c['cookies'], 'cookie', 'cookies')}, every sign-in, "
            "site data, history, tabs and cache"
        )
    if c["chrome"] and c["downloads"]:
        lines.append(f"Downloads: {count(c['downloads'], 'file', 'files')}")
    if files and c["work_files"]:
        lines.append(f"the work folder: {count(c['work_files'], 'file', 'files')}")
    return lines


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("instance")
    parser.add_argument("--files", action="store_true", help="also empty /home/worker/work")
    parser.add_argument("--keep-copy", action="store_true", help="move it all aside instead of deleting it")
    parser.add_argument("--restore", action="store_true", help="put the newest copy back")
    parser.add_argument("--dry-run", action="store_true", help="only show what would go")
    args = parser.parse_args()
    if args.restore and (args.keep_copy or args.files):
        sys.exit("--restore puts back what the copy has: no --keep-copy or --files with it.")

    name = desktop_name(args.instance)
    survey = run_remote(args.instance, "survey", False)
    status, before = survey["status"] or {}, survey["before"]
    running = status.get("busy") or (status.get("background") or {}).get("running")
    state = "busy: a task is running" if running else "idle" if status else "agentd isn't answering"
    chrome = f", Chrome as {before['chrome_user']}" if before["chrome"] else ", no Chrome service"
    print(f"Desktop {name} ({args.instance}), agentd {status.get('version', '?')}, {state}{chrome}.")
    if before["copies"]:
        print(f"Kept copies: {', '.join(before['copies'])}")
    if before["unknown"]:
        print(f"Also in /var/lib/agentd, left alone: {', '.join(before['unknown'])}")

    if args.restore:
        if not before["copies"]:
            sys.exit("No copy to restore (make one with --keep-copy).")
        print(f"\nWill put back the copy from {before['copies'][-1]}, replacing what's there now:")
        action = "restore"
    else:
        print("\nWill " + ("move aside, to /var/lib/agentd-reset/:" if args.keep_copy else "delete:"))
        action = "keep-copy" if args.keep_copy else "delete"
    for line in describe(before, args.files) or ["(nothing: it's already empty)"]:
        print(f"  - {line}")
    if not args.restore:
        print("Kept: Chrome's policy, push notifications, agentd's config.")
        if not args.keep_copy:
            print("This can't be undone (--keep-copy moves it aside instead).")
    if args.dry_run:
        return
    if running:
        sys.exit("\nA task is running: wait for it, or stop it in the app, then run this again.")
    if input(f"\nType the desktop's name ({name}) to go ahead: ").strip() != name:
        sys.exit("Nothing changed.")

    done = run_remote(args.instance, action, args.files)
    if "refused" in done:
        sys.exit(f"Nothing changed: {done['refused']}.")
    after, up = done["after"], done["status"]
    if "restored" in done:
        print(f"Restored the copy from {done['restored']}.")
    elif "copy" in done:
        print(f"Moved aside to {done['copy']} (make agentd-reset RESTORE=1 puts it back).")
    if before["chrome"] and not done["chrome_running"]:
        print("Chrome didn't start again: check it in the viewer (make viewer).")
    if not up:
        sys.exit("agentd didn't come back up: check it with make agentd-logs.")
    print(
        f"agentd {up.get('version', '?')} is running again with "
        f"{count(after['messages'] or 0, 'chat message', 'chat messages')}, "
        f"{count(after['memories'] or 0, 'memory', 'memories')}, "
        f"{count(after['schedules'] or 0, 'schedule', 'schedules')} and "
        f"{count(after['saved_keys'], 'saved key', 'saved keys')}."
    )
    if args.restore:
        print("Chrome has its sign-ins back. Open the app again to load the chat.")
    else:
        print("Chrome is signed out of everything: sign in again in the viewer (make viewer) where the agent needs it.")
        print("Open the app again to see the empty chat.")


if __name__ == "__main__":
    main()
