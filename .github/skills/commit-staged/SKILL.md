---
 name: commit-staged
 description: Commit all staged files with an accurate, informative commit message. Use when asked to commit, make a commit, or save changes.
 ---
 
 1. Run `git diff --cached` to review all staged changes.
 2. Analyze the changes to understand what was modified and why.
 3. Write a commit message following conventional commit style:
    - A concise subject line (≤72 chars) summarizing the change
    - A blank line followed by a body explaining what and why
    - Include the Co-authored-by trailer
 4. Let the user review the commit message and make edits if necessary before finalizing the commit. Never commit without user approval of the message.
 4. Commit with `git commit -m "..."`.