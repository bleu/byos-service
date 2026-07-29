# Vendored forge artifacts

`Escrow.json` is the forge build artifact (ABI + creation bytecode, trimmed)
from [`bleu/byos-contracts`](https://github.com/bleu/byos-contracts) at commit
`ac1e810b8867e5e442d777c05e72e5cb99862c59` (main). The e2e harness deploys it
via the CREATE2 singleton factory at suite start — ADR-0009's documented
fallback while the contracts churn pre-audit.

Only the Escrow artifact is vendored: its constructor deploys the
TrampolineFactory itself (which in turn embeds the Trampoline creation code),
so the factory needs no separate deployment; the harness reads its address
back from `escrow.TRAMPOLINE_FACTORY()`.

Regenerate when the contracts change:

```sh
cd byos-contracts && forge build
jq '{abi, bytecode}' out/Escrow.sol/Escrow.json \
  > byos-service/crates/e2e/testdata/artifacts/Escrow.json
```

Update the commit hash above and keep `crates/byos-common/abis/` (ABI-only
bindings) in sync if the interfaces changed.
