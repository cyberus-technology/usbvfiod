# Code Guidelines

This document lists code design and style choices that developers
should adhere to when modifying or extending the source code.

## DMA Address Calculations 

When calculating DMA addresses to access guest memory, you should use
wrap-around calculations such as `u64::wrapping_add`.

Overflows in addresses calculations should not occur during normal
operation, only when the USB driver in the guest wants to shoot
itself in the foot. In such cases, we want to behave like a physical
XHCI controller and wrap around.

Explicitly treating the calculations as wrapping aligns the behavior between debug and release builds.

## Trait Derivations

Deriving standard traits improves debuggability, testability, and code clarity. While not strictly mandatory, developers are encouraged to apply them where safe and semantically appropriate.

- **Always derive `Debug`**  
  Include `#[derive(Debug)]` for all structs and enums to enable diagnostic output and logging.

- **Derive `Clone` or `Copy`**  
  Use `Copy` only for small types with trivial duplication semantics (e.g., numeric types). Use `Clone` for safe but potentially more complex duplication.  
  _Avoid both_ if the type encapsulates ownership-sensitive resources (e.g., file handles, raw pointers, custom lifetimes).

- **Derive `PartialEq`, `Eq`**  
  Enable equality checks, especially useful in test assertions and control logic. Only add when comparisons are semantically meaningful.

- **Derive `Default`**  
  Provide `Default` when a clear and unambiguous zero-configuration or initial state exists.

Derives may be added proactively for likely future needs (e.g., test support or logging), provided they don’t introduce ambiguity or misuse.

### Recommended Derive Order

For consistency and readability, follow this trait order when deriving multiple traits:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
```