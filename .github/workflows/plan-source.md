---
on:
  workflow_dispatch:
    inputs:
      target:
        description: 'Target system'
        required: true
        type: string
imports:
  - ../agents/source-planner.md
model: claude-sonnet-4.5
engine:
  id: copilot
permissions:
  copilot-requests: write
  contents: read
  issues: read
  pull-requests: read
tools:
  web-fetch:
  web-search:
  github:
safe-outputs:
  create-pull-request:
    draft: true
    expires: 14d
---

# source-planner

Write a plan for a ${{ github.event.inputs.target }} source and save it to a file in my workspace so that I can edit it.