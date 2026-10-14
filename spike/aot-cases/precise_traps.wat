;; At a trap, memory must hold exactly the stores before it, as Wasm says.
;; LLVM may merge adjacent stores or move them past a read it thinks can't
;; fail; with guard pages the read faults, so these check it didn't.
(module
  (memory 1 1)
  ;; Two byte stores around a faulting load: only the first may land.
  (func (export "merge") (result i32)
    (i32.store8 (i32.const 100) (i32.const 1))
    (drop (i32.load (i32.const 70000)))
    (i32.store8 (i32.const 101) (i32.const 2))
    (i32.const 0))
  ;; A store, then a faulting store: the first must land.
  (func (export "store_then_trap") (result i32)
    (i32.store (i32.const 200) (i32.const 0x11223344))
    (i32.store (i32.const 65535) (i32.const 1))
    (i32.const 0))
  ;; Stores in a loop, then a fault: every iteration's store landed.
  (func (export "loop_then_trap") (result i32) (local i32)
    (loop
      (i32.store8 (i32.add (i32.const 300) (local.get 0)) (local.get 0))
      (local.set 0 (i32.add (local.get 0) (i32.const 1)))
      (br_if 0 (i32.lt_u (local.get 0) (i32.const 64))))
    (drop (i64.load (i32.const 65530)))
    (i32.store8 (i32.const 400) (i32.const 9))
    (i32.const 0))
  ;; A store whose value is overwritten later: the trap comes in between.
  (func (export "dead_store") (result i32)
    (i32.store (i32.const 500) (i32.const 7))
    (drop (i32.load (i32.const 99999)))
    (i32.store (i32.const 500) (i32.const 8))
    (i32.const 0))
  ;; Same, with the fault in a function call.
  (func $boom (drop (i32.load (i32.const 99999))))
  (func (export "call_trap") (result i32)
    (i32.store (i32.const 600) (i32.const 7))
    (call $boom)
    (i32.store (i32.const 600) (i32.const 8))
    (i32.const 0))
  ;; A division by zero (an explicit check, not a fault) between stores.
  (func (export "div_trap") (param i32) (result i32)
    (i32.store8 (i32.const 700) (i32.const 1))
    (drop (i32.div_u (i32.const 1) (local.get 0)))
    (i32.store8 (i32.const 701) (i32.const 2))
    (i32.const 0)))
