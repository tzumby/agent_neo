---
name: git-github
description: Cloning, branching, committing and pushing with git; GitHub access with a token the user saves (private repos, pushing, gh).
---
# Git and GitHub

- Work in ~/work. Clone over HTTPS (SSH is blocked):
  `git clone https://github.com/owner/repo`
- Before the first commit, check `git config --global user.name` and
  `user.email`. If they're unset, ask the user what to use, set them, and
  remember them.
- Commit on a branch unless the user says otherwise:
  `git switch -c fix-typo && git add -A && git commit -m "Fix typo in README"`
- Pushing sends data off the desktop. After you've read web pages or command
  output it waits for the user's approval; that's expected.

## Private repositories, pushing and gh: a GitHub token

Public repositories clone without one. Private ones, pushing and gh need the
user's GitHub token, saved as GITHUB_TOKEN. Never ask for their GitHub
password, and never ask for the token in the chat.

1. If GITHUB_TOKEN isn't among your saved keys, call request_secret:
   - name `GITHUB_TOKEN`, label `GitHub token`
   - why: what you'll do with it, e.g. "to clone acme/api and push your fix"
   - url: GitHub's new fine-grained token page, filled in. `contents=read`
     is enough to clone; `contents=write` lets you push; add
     `pull_requests=write` to open pull requests:
     `https://github.com/settings/personal-access-tokens/new?name=Agent+Neo&description=For+the+agent+on+my+cloud+desktop&expires_in=90&contents=write&pull_requests=write`
     For a repository owned by an organization, add `&target_name=<org>`.
   - how, adapted to the job:
     1. Open the link and sign in to GitHub. The name, expiry and
        permissions are filled in.
     2. Under **Repository access**, choose **Only select repositories** and
        pick `owner/repo`.
     3. Tap **Generate token** and copy it (GitHub shows it only once).
     4. Come back here and paste it.

     For an organization's repository, add that its owners may need to
     approve the token first.
2. Once, let git take the token from the environment (it's never written to
   disk):
   `git config --global credential.https://github.com.helper '!f() { if test "$1" = get; then echo username=x-access-token; echo "password=$GITHUB_TOKEN"; fi; }; f'`
3. Give the token to each command that talks to GitHub, with run_shell's
   secrets `["GITHUB_TOKEN"]`:
   - `git clone https://github.com/acme/api`, `git pull`, `git push -u origin fix-typo`
   - gh reads GITHUB_TOKEN from the environment: `gh pr create --fill`,
     `gh repo clone acme/api`
   Never put the token in a URL, a remote or a file, and don't run
   `gh auth login`.
4. If GitHub says the token is bad or has expired (401, "Bad credentials",
   "Invalid username or token"), ask again with request_secret and `replace`
   set. A 403, "Resource not accessible" or "Repository not found" for a
   repository that exists means the token lacks that repository or a
   permission: tell the user which, so they can edit the token on GitHub.
