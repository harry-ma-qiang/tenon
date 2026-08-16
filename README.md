# Tenon

White-label functional port of the Cordis kernel to Elixir/OTP. Processes are
fibers, Registry is reflect, supervision is cascading dispose.

Design reference: Cordis (MIT), see NOTICE. All design, decisions and phase
plans live in NOTES.md.

Gates:

```
mix deps.get && mix compile --warnings-as-errors && mix format --check-formatted && mix credo --strict && mix test
```
