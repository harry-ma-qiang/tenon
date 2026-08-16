# Tenon

White-label functional port of the Cordis microkernel to Erlang/OTP. Processes are
fibers, ETS is the registry, supervision is cascading dispose.

The kernel is one Erlang module: [`kernel/src/tenon.erl`](kernel/src/tenon.erl).
Read [`kernel/README.md`](kernel/README.md) for the architecture, the API, the wire
protocol and the hot swap procedure. Everything else — config loader, schema, bridges —
is a plugin outside the kernel.

Design reference: Cordis (MIT), see NOTICE. All decisions and phase plans live in
NOTES.md.

Gates:

```
cd kernel && mix compile && mix format --check-formatted && mix test
```
