# Cross-component handoffs

Use one Markdown file per handoff. The sending agent must provide enough information for the receiving agent to continue without rediscovering the work.

```text
Task:
Owner:
Status:
Firefox commit and source paths:
Firefox test paths:
Wild Buzzard paths changed:
Contract added or changed:
Tests run and results:
Parity evidence:
Known behavioral differences:
Unsafe or FFI introduced:
Licenses and provenance:
Provider or network implications:
Blocked on:
Recommended next action:
```

Do not use a handoff merely to say that integration is needed. Identify the exact API, behavior, caller, receiving owner, and failing or missing test.
