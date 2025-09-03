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

## `unwrap()` Safety Explanations

`unwrap()` should only be used when the operation is guaranteed to succeed based on program logic, mathematical invariants, or architectural contracts. Every unwrap() must be justified with a safety comment or fall into a well-established safe category.

### Categories of Unwrap Usage

#### 1. Always Safe - No Comment Required

**1.1 Mutex Lock Operations**

```rust
// No safety comment needed
self.data.lock().unwrap()
```
**Rationale**: The `lock()` operation itself filters out most error conditions before reaching `unwrap()`. Mutex poisoning only occurs when a thread panics while holding the lock, which indicates a serious program error. In such cases, propagating the panic via `unwrap()` is the correct behavior. The error handling is already done at the `lock()` level, making `unwrap()` safe in this context.

**1.2 Test Code**
```rust
#[cfg(test)]
fn test_something() {
    let result = some_operation().unwrap(); // OK in tests
}
```
**Rationale**: In test environments, process crashes are acceptable and even desirable for debugging. Test failures should be immediate and obvious. Therefore `unwrap()` is an appropriate choice for its simplicity and clear failure indication.

#### 2. Safe with Required Comments

When `unwrap()` is safe but not obviously so, add a SAFETY comment explaining why:

```rust
// SAFETY: usize always fits in u64 on all supported platforms
let size_u64: u64 = array.len().try_into().unwrap();

// SAFETY: cur_offset >= request_start is guaranteed by assertion above
let data_offset: usize = (self.cur_offset - self.request_start).try_into().unwrap();

// SAFETY: All RequestSize variants are non-zero
Self::new(r as u64).unwrap()
```

#### 3. Avoid These Cases

```rust
// BAD: u64 may not fit in usize on 32-bit systems
let offset: usize = addr.try_into().unwrap();

// BAD: External data can be invalid
let value = user_input.parse::<u32>().unwrap();
let file_content = std::fs::read_to_string(path).unwrap();
```

**Instead**: Use bounds checking or proper error handling.

#### 4. Uncertain Cases

When `unwrap()` safety is questionable, mark it for review:

```rust
// FIXME: This unwrap assumes req.addr fits in usize and is within bounds.
// Need architectural clarification on address validation responsibilities.
let off: usize = req.addr.try_into().unwrap();
```

Use `FIXME` comments to mark questionable `unwrap()` calls for code review discussion.