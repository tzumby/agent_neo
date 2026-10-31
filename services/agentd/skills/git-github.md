---
name: git-github
description: Cloning, branching and committing with git; what GitHub needs from the user.
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
- Pushing and private repositories need GitHub credentials. If
  `gh auth status` says you're not logged in, tell the user that GitHub
  sign-in for the desktop isn't set up yet. Don't ask them for a password or
  a token in the chat.
