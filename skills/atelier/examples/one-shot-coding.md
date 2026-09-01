# One-shot coding agent

Run the coding agent inside the session rather than attaching Atelier beside
an unrelated checkout:

```sh
cd ~/workspaces/payments
atelier manifest
atelier run   --summary "Fix webhook replay ordering and verify the regression"   --land   -- omp
atelier journal
```

`omp` starts in the session working copy. Replace it with the user's coding
agent or a bounded command. Drop `--land` when a human must review the request
or when success is not enough to approve it.

If the command fails, use `atelier sessions` to find the retained session,
inspect it with `atelier session diff <session>`, and continue in its printed
working copy. Starting a second session would split one body of work across
two histories.
