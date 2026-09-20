//! Executable component tests for return allocation, signed budgets and post-return cleanup.

use super::*;
use ed25519_dalek::Signer;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use wasm_encoder::{
    Alias, BlockType, CanonicalFunctionSection, CanonicalOption, CodeSection,
    ComponentAliasSection, ComponentExportKind, ComponentExportSection, ComponentTypeSection,
    ComponentValType, ConstExpr, DataSection, ExportKind, ExportSection, Function, FunctionSection,
    InstanceSection, Instruction, MemArg, MemorySection, MemoryType, Module, ModuleArg,
    ModuleSection, PrimitiveValType, TypeSection, ValType,
};

thread_local! {
    /// Tracks allocations of one exact size on the invoking test thread only.
    static OUTPUT_ALLOCATIONS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

/// Observes return-sized allocations without changing the system allocator's behavior.
struct ObservedAllocator;

/// Counts matching allocation requests without allocating or panicking in allocator callbacks.
fn observe_allocation(size: usize) {
    let _ = OUTPUT_ALLOCATIONS.try_with(|state| {
        let (target, count) = state.get();
        if target != 0 && size == target {
            state.set((target, count.saturating_add(1)));
        }
    });
}

// SAFETY: All allocation operations forward their original arguments to System.
/// Delegates every operation to System while counting the selected allocation size.
unsafe impl GlobalAlloc for ObservedAllocator {
    /// Records allocations before forwarding the original layout to System.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        observe_allocation(layout.size());
        // SAFETY: The caller provides the layout required by GlobalAlloc.
        unsafe { System.alloc(layout) }
    }

    /// Records zeroed allocations and preserves System's initialization contract.
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        observe_allocation(layout.size());
        // SAFETY: The caller provides the layout required by GlobalAlloc.
        unsafe { System.alloc_zeroed(layout) }
    }

    /// Forwards deallocation with the same pointer and allocation layout.
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: The caller guarantees this pointer and layout describe a live allocation.
        unsafe { System.dealloc(pointer, layout) }
    }

    /// Records a requested new allocation size before delegating reallocation.
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        observe_allocation(size);
        // SAFETY: GlobalAlloc's caller supplies a live pointer, its layout and a valid new size.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

/// Installs allocation observation only in this crate's unit-test executable.
#[global_allocator]
static TEST_ALLOCATOR: ObservedAllocator = ObservedAllocator;

/// Selects guest cleanup behavior after returning a borrowed byte list.
#[derive(Clone, Copy)]
enum Cleanup {
    /// Overwrites returned bytes to prove the host copies them before cleanup.
    Overwrite,
    /// Traps so cleanup failures cannot be mistaken for successful output.
    Trap,
    /// Loops until the signed runtime deadline interrupts cleanup.
    Spin,
}

/// Encodes one-memory binary components with a configurable returned range and cleanup.
fn return_component(pointer: u32, length: u32, cleanup: Cleanup) -> Vec<u8> {
    let mut module = Module::new();
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32; 4], [ValType::I32]);
    types.ty().function([ValType::I32; 2], [ValType::I32]);
    types.ty().function([ValType::I32], []);
    types.ty().function([], [ValType::I32]);
    module.section(&types);
    let mut functions = FunctionSection::new();
    functions.function(0).function(1).function(2).function(3);
    module.section(&functions);
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: Some(1),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memories);
    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("realloc", ExportKind::Func, 0);
    exports.export("invoke", ExportKind::Func, 1);
    exports.export("cleanup", ExportKind::Func, 2);
    exports.export("marker", ExportKind::Func, 3);
    module.section(&exports);

    let mut code = CodeSection::new();
    // Tests pass empty payloads; reserve a separate aligned address for canonical lowering.
    let mut realloc = Function::new([]);
    realloc.instruction(&Instruction::I32Const(65_000));
    realloc.instruction(&Instruction::End);
    code.function(&realloc);
    let mut invoke = Function::new([]);
    invoke.instruction(&Instruction::I32Const(0));
    invoke.instruction(&Instruction::End);
    code.function(&invoke);
    let mut post_return = Function::new([]);
    match cleanup {
        Cleanup::Overwrite => {
            post_return.instruction(&Instruction::I32Const(16));
            post_return.instruction(&Instruction::I32Const(0x5a5a5a5a));
            post_return.instruction(&Instruction::I32Store(MemArg {
                offset: 0,
                align: 2,
                memory_index: 0,
            }));
        }
        Cleanup::Trap => {
            post_return.instruction(&Instruction::Unreachable);
        }
        Cleanup::Spin => {
            post_return.instruction(&Instruction::Loop(BlockType::Empty));
            post_return.instruction(&Instruction::Br(0));
            post_return.instruction(&Instruction::End);
        }
    }
    post_return.instruction(&Instruction::End);
    code.function(&post_return);
    let mut marker = Function::new([]);
    marker.instruction(&Instruction::I32Const(16));
    marker.instruction(&Instruction::I32Load(MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    }));
    marker.instruction(&Instruction::End);
    code.function(&marker);
    module.section(&code);
    let mut data = DataSection::new();
    let descriptor = [pointer.to_le_bytes(), length.to_le_bytes()].concat();
    data.active(0, &ConstExpr::i32_const(0), descriptor);
    data.active(0, &ConstExpr::i32_const(16), *b"ABCD");
    module.section(&data);

    let mut component = wasm_encoder::Component::new();
    component.section(&ModuleSection(&module));
    let mut instances = InstanceSection::new();
    instances.instantiate(0, std::iter::empty::<(&str, ModuleArg)>());
    component.section(&instances);
    let mut aliases = ComponentAliasSection::new();
    for (kind, name) in [
        (ExportKind::Memory, "memory"),
        (ExportKind::Func, "realloc"),
        (ExportKind::Func, "invoke"),
        (ExportKind::Func, "cleanup"),
        (ExportKind::Func, "marker"),
    ] {
        aliases.alias(Alias::CoreInstanceExport {
            instance: 0,
            kind,
            name,
        });
    }
    component.section(&aliases);
    let mut component_types = ComponentTypeSection::new();
    component_types.defined_type().list(PrimitiveValType::U8);
    component_types
        .function()
        .params([("payload", ComponentValType::Type(0))])
        .result(Some(ComponentValType::Type(0)));
    component_types
        .function()
        .params(std::iter::empty::<(&str, ComponentValType)>())
        .result(Some(PrimitiveValType::U32.into()));
    component.section(&component_types);
    let mut canonical = CanonicalFunctionSection::new();
    canonical.lift(
        1,
        1,
        [
            CanonicalOption::Memory(0),
            CanonicalOption::Realloc(0),
            CanonicalOption::PostReturn(2),
        ],
    );
    canonical.lift(3, 2, []);
    component.section(&canonical);
    let mut exports = ComponentExportSection::new();
    exports.export("invoke", ComponentExportKind::Func, 0, None);
    exports.export("cleanup-marker", ComponentExportKind::Func, 1, None);
    component.section(&exports);
    component.finish()
}

/// Signs the exact binary and configured resource envelope before invoking the public entrypoint.
fn invoke_signed(component: &[u8], limits: ResourceLimits) -> Result<Vec<u8>, SandboxError> {
    let (trust, mut manifest, key) = tests::signed_fixture(component);
    manifest.limits = limits;
    manifest.signature =
        STANDARD_NO_PAD.encode(key.sign(&manifest.signing_bytes().unwrap()).to_bytes());
    ComponentSandbox::new()?.invoke(
        &trust,
        &manifest,
        component,
        InvocationMediators::default(),
        &[],
    )
}

/// Observes no host return-sized allocation on rejection, with accepted output as a positive control.
#[test]
fn oversized_signed_return_is_rejected_before_host_allocation() {
    // A distinct non-power-of-two size separates the result buffer from runtime bookkeeping.
    const RETURN_BYTES: usize = 60_001;
    let component = return_component(16, RETURN_BYTES as u32, Cleanup::Overwrite);
    for (limit, accepted) in [(RETURN_BYTES - 1, false), (RETURN_BYTES, true)] {
        OUTPUT_ALLOCATIONS.with(|state| state.set((RETURN_BYTES, 0)));
        let result = invoke_signed(
            &component,
            ResourceLimits {
                output_bytes: limit,
                ..ResourceLimits::default()
            },
        );
        let allocations = OUTPUT_ALLOCATIONS.with(|state| state.replace((0, 0)).1);
        if accepted {
            let output = result.unwrap();
            assert_eq!(output.len(), RETURN_BYTES);
            assert_eq!(&output[..4], b"ABCD");
            assert_eq!(
                allocations, 1,
                "positive control must observe the host return copy"
            );
        } else {
            assert!(matches!(result, Err(SandboxError::OutputLimitExceeded)));
            assert_eq!(
                allocations, 0,
                "rejected output must never reserve a host return buffer"
            );
        }
    }
}

/// Accepts empty and exact-limit results and copies bytes before cleanup overwrites guest memory.
#[test]
fn signed_return_accepts_empty_and_exact_limit_before_cleanup() {
    for length in [0, 4] {
        let component = return_component(16, length, Cleanup::Overwrite);
        let output = invoke_signed(
            &component,
            ResourceLimits {
                output_bytes: 4,
                ..ResourceLimits::default()
            },
        )
        .unwrap();
        assert_eq!(output, &b"ABCD"[..length as usize]);
    }
}

/// Rejects malformed guest ranges before a host output copy can be attempted.
#[test]
fn signed_return_rejects_out_of_bounds_range() {
    let component = return_component(65_535, 4, Cleanup::Overwrite);
    assert!(matches!(
        invoke_signed(&component, ResourceLimits::default()),
        Err(SandboxError::Wasmtime(_))
    ));
}

/// Propagates cleanup traps on accepted output and preserves an earlier output-limit rejection.
#[test]
fn signed_return_cleanup_traps_do_not_report_success() {
    let component = return_component(16, 4, Cleanup::Trap);
    assert!(matches!(
        invoke_signed(&component, ResourceLimits::default()),
        Err(SandboxError::Wasmtime(_))
    ));
    assert!(matches!(
        invoke_signed(
            &component,
            ResourceLimits {
                output_bytes: 3,
                ..ResourceLimits::default()
            }
        ),
        Err(SandboxError::OutputLimitExceeded)
    ));
}

/// Interrupts looping cleanup using the signed epoch or absolute wall-clock deadline.
#[test]
fn signed_return_cleanup_is_interrupted() {
    let component = return_component(16, 4, Cleanup::Spin);
    let limits = ResourceLimits {
        output_bytes: 4,
        timeout_ms: 20,
        fuel: u64::MAX,
        ..ResourceLimits::default()
    };
    let started = Instant::now();
    match invoke_signed(&component, limits) {
        Err(SandboxError::DeadlineExceeded) => {}
        Err(SandboxError::Wasmtime(error)) => {
            assert_eq!(
                error.downcast_ref::<wasmtime::Trap>(),
                Some(&wasmtime::Trap::Interrupt)
            );
        }
        other => panic!("cleanup must be interrupted: {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// Preserves cumulative charges and deadlines while observing cleanup after rejected output.
#[test]
fn borrowed_copy_respects_cumulative_budget_and_expired_deadline() {
    for (length, prior, expired, accepted) in [
        (4, 1, false, false),
        (0, 4, false, true),
        (4, 0, true, false),
    ] {
        let bytes = return_component(16, length, Cleanup::Overwrite);
        let (trust, manifest, _) = tests::signed_fixture(&bytes);
        let sandbox = ComponentSandbox::new().unwrap();
        let component = sandbox.load_component(&trust, &manifest, &bytes).unwrap();
        let limits = ResourceLimits {
            output_bytes: 4,
            ..ResourceLimits::default()
        };
        let mut budget = InvocationBudget::new(limits.clone()).unwrap();
        budget.charge_output(prior).unwrap();
        if expired {
            budget.deadline = Instant::now();
        }
        let mut store = Store::new(
            &sandbox.engine,
            SandboxState {
                granted: BTreeSet::new(),
                http: None,
                broker: None,
                budget,
                store_limits: component_store_limits(limits.memory_bytes),
            },
        );
        store.limiter(|state| &mut state.store_limits);
        store.set_fuel(limits.fuel).unwrap();
        store.set_epoch_deadline(epoch_deadline_ticks(limits.timeout_ms));
        let instance = Linker::new(&sandbox.engine)
            .instantiate(&mut store, &component)
            .unwrap();
        let marker = instance
            .get_typed_func::<(), (u32,)>(&mut store, "cleanup-marker")
            .unwrap();
        assert_eq!(
            marker.call(&mut store, ()).unwrap().0,
            u32::from_le_bytes(*b"ABCD")
        );
        marker.post_return(&mut store).unwrap();
        let result = call_component_output(&mut store, &instance, &[]);
        assert_eq!(
            marker.call(&mut store, ()).unwrap().0,
            u32::from_le_bytes(*b"ZZZZ")
        );
        marker.post_return(&mut store).unwrap();
        if accepted {
            assert_eq!(result.unwrap(), Vec::<u8>::new());
        } else if expired {
            assert!(matches!(result, Err(SandboxError::DeadlineExceeded)));
        } else {
            assert!(matches!(result, Err(SandboxError::OutputLimitExceeded)));
        }
    }
}
