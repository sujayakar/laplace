#pragma once
// Use clang builtins for atomics (single-threaded, so relaxed is fine)
typedef enum {
    memory_order_relaxed = __ATOMIC_RELAXED,
    memory_order_consume = __ATOMIC_CONSUME,
    memory_order_acquire = __ATOMIC_ACQUIRE,
    memory_order_release = __ATOMIC_RELEASE,
    memory_order_acq_rel = __ATOMIC_ACQ_REL,
    memory_order_seq_cst = __ATOMIC_SEQ_CST,
} memory_order;

// Use compiler's native _Atomic support
// Don't redefine _Atomic — clang supports it natively

#define atomic_load_explicit(obj, order) __atomic_load_n(obj, order)
#define atomic_store_explicit(obj, val, order) __atomic_store_n(obj, val, order)
#define atomic_fetch_add_explicit(obj, val, order) __atomic_fetch_add(obj, val, order)
#define atomic_fetch_sub_explicit(obj, val, order) __atomic_fetch_sub(obj, val, order)
#define atomic_fetch_or_explicit(obj, val, order) __atomic_fetch_or(obj, val, order)
#define atomic_fetch_and_explicit(obj, val, order) __atomic_fetch_and(obj, val, order)
#define atomic_compare_exchange_strong_explicit(obj, expected, desired, succ, fail) \
    __atomic_compare_exchange_n(obj, expected, desired, 0, succ, fail)
#define atomic_compare_exchange_weak_explicit(obj, expected, desired, succ, fail) \
    __atomic_compare_exchange_n(obj, expected, desired, 1, succ, fail)

#define atomic_load(obj) __atomic_load_n(obj, __ATOMIC_SEQ_CST)
#define atomic_store(obj, val) __atomic_store_n(obj, val, __ATOMIC_SEQ_CST)
#define atomic_fetch_add(obj, val) __atomic_fetch_add(obj, val, __ATOMIC_SEQ_CST)
#define atomic_fetch_sub(obj, val) __atomic_fetch_sub(obj, val, __ATOMIC_SEQ_CST)
#define atomic_fetch_or(obj, val) __atomic_fetch_or(obj, val, __ATOMIC_SEQ_CST)
#define atomic_fetch_and(obj, val) __atomic_fetch_and(obj, val, __ATOMIC_SEQ_CST)

typedef _Atomic(int) atomic_int;
typedef _Atomic(unsigned int) atomic_uint;

#define ATOMIC_VAR_INIT(value) (value)
#define atomic_init(obj, val) do { *(obj) = (val); } while(0)
