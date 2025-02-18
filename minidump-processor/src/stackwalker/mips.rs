use std::collections::HashSet;
use std::convert::TryFrom;

use minidump::format::ContextFlagsCpu;
use minidump::{
    CpuContext, Endian, MinidumpContext, MinidumpContextValidity, MinidumpMemory,
    MinidumpModuleList, MinidumpRawContext,
};
use scroll::ctx::{SizeWith, TryFromCtx};
use tracing::trace;

use crate::stackwalker::unwind::Unwind;
use crate::stackwalker::CfiStackWalker;
use crate::{FrameTrust, StackFrame, SymbolProvider, SystemInfo};

type MipsContext = minidump::format::CONTEXT_MIPS;
type Pointer = <MipsContext as CpuContext>::Register;

const STACK_POINTER: &str = "sp";
const PROGRAM_COUNTER: &str = "pc";
const CALLEE_SAVED_REGS: &[&str] = &[
    "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "gp", "sp", "fp",
];

async fn get_caller_by_cfi<'a, C, P>(
    ctx: &'a C,
    callee: &'a StackFrame,
    grand_callee: Option<&'a StackFrame>,
    stack_memory: &'a MinidumpMemory<'_>,
    modules: &'a MinidumpModuleList,
    symbol_provider: &'a P,
) -> Option<StackFrame>
where
    P: SymbolProvider + Sync,
    // all these bounds are essentially duplicated from `CfiStackWalker` :-(
    C: CpuContext + IntoRawContext + Clone + Send + Sync,
    C::Register: TryFrom<u64>,
    u64: TryFrom<C::Register>,
    C::Register: TryFromCtx<'a, Endian, [u8], Error = scroll::Error> + SizeWith<Endian>,
{
    trace!("trying cfi");
    let valid = &callee.context.valid;
    let _last_sp = ctx.get_register(STACK_POINTER, valid)?;
    let module = modules.module_at_address(callee.instruction)?;
    let grand_callee_parameter_size = grand_callee.and_then(|f| f.parameter_size).unwrap_or(0);
    let has_grand_callee = grand_callee.is_some();

    let mut stack_walker = CfiStackWalker {
        instruction: callee.instruction,
        has_grand_callee,
        grand_callee_parameter_size,

        callee_ctx: ctx,
        callee_validity: valid,

        // Default to forwarding all callee-saved regs verbatim.
        // The CFI evaluator may clear or overwrite these values.
        // The stack pointer and instruction pointer are not included.
        caller_ctx: ctx.clone(),
        caller_validity: callee_forwarded_regs(valid),

        stack_memory,
    };

    symbol_provider
        .walk_frame(module, &mut stack_walker)
        .await?;
    let caller_pc = stack_walker.caller_ctx.get_register_always(PROGRAM_COUNTER);
    let caller_sp = stack_walker.caller_ctx.get_register_always(STACK_POINTER);

    trace!(
        "cfi evaluation was successful -- caller_pc: 0x{caller_pc:016x}, caller_sp: 0x{caller_sp:016x}"
    );

    // Do absolutely NO validation! Yep! As long as CFI evaluation succeeds
    // (which does include pc and sp resolving), just blindly assume the
    // values are correct. I Don't Like This, but it's what breakpad does and
    // we should start with a baseline of parity.

    let context = MinidumpContext {
        raw: stack_walker.caller_ctx.into_ctx(),
        valid: MinidumpContextValidity::Some(stack_walker.caller_validity),
    };
    Some(StackFrame::from_context(context, FrameTrust::CallFrameInfo))
}

fn callee_forwarded_regs(valid: &MinidumpContextValidity) -> HashSet<&'static str> {
    match valid {
        MinidumpContextValidity::All => CALLEE_SAVED_REGS.iter().copied().collect(),
        MinidumpContextValidity::Some(ref which) => CALLEE_SAVED_REGS
            .iter()
            .filter(|&reg| which.contains(reg))
            .copied()
            .collect(),
    }
}

async fn get_caller_by_scan32<P>(
    ctx: &Mips32Context,
    callee: &StackFrame,
    stack_memory: &MinidumpMemory<'_>,
    modules: &MinidumpModuleList,
    symbol_provider: &P,
) -> Option<StackFrame>
where
    P: SymbolProvider + Sync,
{
    const MAX_STACK_SIZE: u32 = 1024;
    const MIN_ARGS: u32 = 4;
    const POINTER_WIDTH: u32 = 4;
    trace!("trying scan");
    // Stack scanning is just walking from the end of the frame until we encounter
    // a value on the stack that looks like a pointer into some code (it's an address
    // in a range covered by one of our modules). If we find such an instruction,
    // we assume it's a `ra` value that was saved on the stack by the callee in
    // its function prologue, following a `jal` (call) instruction of the caller.
    // The next frame is then assumed to end just before that `ra` value.
    let valid = &callee.context.valid;
    let mut last_sp = ctx.get_register(STACK_POINTER, valid)?;

    let mut count = MAX_STACK_SIZE / POINTER_WIDTH;
    // In case of mips32 ABI the stack frame of a non-leaf function
    // must have a minimum stack frame size for 4 arguments (4 words).
    // Move stack pointer for 4 words to avoid reporting non-existing frames
    // for all frames except the topmost one.
    // There is no way of knowing if topmost frame belongs to a leaf or
    // a non-leaf function.
    if callee.trust != FrameTrust::Context {
        last_sp = last_sp.checked_add(MIN_ARGS * POINTER_WIDTH)?;
        count -= MIN_ARGS;
    }

    for i in 0..count {
        let address_of_pc = last_sp.checked_add(i * POINTER_WIDTH)?;
        let caller_pc: u32 = stack_memory.get_memory_at_address(address_of_pc as u64)?;
        //trace!("unwind: trying addr 0x{address_of_pc:08x}: 0x{caller_pc:08x}");
        if instruction_seems_valid(caller_pc as u64, modules, symbol_provider).await {
            // `ra` is usually saved directly at the bottom of the frame,
            // so sp is just address_of_pc + ptr
            let caller_sp = address_of_pc.checked_add(POINTER_WIDTH)?;

            // Don't do any more validation, and don't try to restore fp
            // (that's what breakpad does!)

            trace!(
                "scan seems valid -- caller_pc: 0x{caller_pc:016x}, caller_sp: 0x{caller_sp:016x}"
            );

            let mut caller_ctx = MipsContext::default();
            caller_ctx.set_register(PROGRAM_COUNTER, caller_pc as u64);
            caller_ctx.set_register(STACK_POINTER, caller_sp as u64);

            let mut valid = HashSet::new();
            valid.insert(PROGRAM_COUNTER);
            valid.insert(STACK_POINTER);

            let context = MinidumpContext {
                raw: MinidumpRawContext::Mips(caller_ctx),
                valid: MinidumpContextValidity::Some(valid),
            };
            return Some(StackFrame::from_context(context, FrameTrust::Scan));
        }
    }

    None
}

async fn get_caller_by_scan64<P>(
    ctx: &MipsContext,
    callee: &StackFrame,
    stack_memory: &MinidumpMemory<'_>,
    modules: &MinidumpModuleList,
    symbol_provider: &P,
) -> Option<StackFrame>
where
    P: SymbolProvider + Sync,
{
    const MAX_STACK_SIZE: u64 = 1024;
    const POINTER_WIDTH: u64 = 8;
    trace!("trying scan");
    // Stack scanning is just walking from the end of the frame until we encounter
    // a value on the stack that looks like a pointer into some code (it's an address
    // in a range covered by one of our modules). If we find such an instruction,
    // we assume it's a `ra` value that was saved on the stack by the callee in
    // its function prologue, following a `jal` (call) instruction of the caller.
    // The next frame is then assumed to end just before that `ra` value.
    let valid = &callee.context.valid;
    let last_sp = ctx.get_register(STACK_POINTER, valid)?;

    let count = MAX_STACK_SIZE / POINTER_WIDTH;

    for i in 0..count {
        let address_of_pc = last_sp.checked_add(i * POINTER_WIDTH)?;
        let caller_pc = stack_memory.get_memory_at_address(address_of_pc)?;
        if instruction_seems_valid(caller_pc, modules, symbol_provider).await {
            // `ra` is usually saved directly at the bottom of the frame,
            // so sp is just address_of_pc + ptr
            let caller_sp = address_of_pc.checked_add(POINTER_WIDTH)?;

            // Don't do any more validation, and don't try to restore fp
            // (that's what breakpad does!)

            trace!(
                "scan seems valid -- caller_pc: 0x{caller_pc:016x}, caller_sp: 0x{caller_sp:016x}"
            );

            let mut caller_ctx = MipsContext::default();
            caller_ctx.set_register(PROGRAM_COUNTER, caller_pc);
            caller_ctx.set_register(STACK_POINTER, caller_sp);

            let mut valid = HashSet::new();
            valid.insert(PROGRAM_COUNTER);
            valid.insert(STACK_POINTER);

            let context = MinidumpContext {
                raw: MinidumpRawContext::Mips(caller_ctx),
                valid: MinidumpContextValidity::Some(valid),
            };
            return Some(StackFrame::from_context(context, FrameTrust::Scan));
        }
    }

    None
}

async fn instruction_seems_valid<P>(
    instruction: Pointer,
    modules: &MinidumpModuleList,
    symbol_provider: &P,
) -> bool
where
    P: SymbolProvider + Sync,
{
    if instruction < 0x1000 {
        return false;
    }

    super::instruction_seems_valid_by_symbols(instruction as u64, modules, symbol_provider).await
}

#[async_trait::async_trait]
impl Unwind for MipsContext {
    async fn get_caller_frame<P>(
        &self,
        callee: &StackFrame,
        grand_callee: Option<&StackFrame>,
        stack_memory: Option<&MinidumpMemory<'_>>,
        modules: &MinidumpModuleList,
        _system_info: &SystemInfo,
        syms: &P,
    ) -> Option<StackFrame>
    where
        P: SymbolProvider + Sync,
    {
        let ctx = Mips32Context::try_from(self.clone());
        let stack = stack_memory.as_ref()?;

        // .await doesn't like closures, so don't use Option chaining
        let mut frame = None;
        if frame.is_none() {
            match &ctx {
                Ok(mips32) => {
                    frame =
                        get_caller_by_cfi(mips32, callee, grand_callee, stack, modules, syms).await
                }
                Err(mips64) => {
                    frame =
                        get_caller_by_cfi(mips64, callee, grand_callee, stack, modules, syms).await
                }
            }
        }
        if frame.is_none() {
            match &ctx {
                Ok(mips32) => {
                    frame = get_caller_by_scan32(mips32, callee, stack, modules, syms).await
                }
                Err(mips64) => {
                    frame = get_caller_by_scan64(mips64, callee, stack, modules, syms).await
                }
            }
        }
        let mut frame = frame?;

        // We now check the frame to see if it looks like unwinding is complete,
        // based on the frame we computed having a nonsense value. Returning
        // None signals to the unwinder to stop unwinding.

        // if the instruction is within the first ~page of memory, it's basically
        // null, and we can assume unwinding is complete.
        if frame.context.get_instruction_pointer() < 4096 {
            trace!("instruction pointer was nullish, assuming unwind complete");
            return None;
        }

        // If the new stack pointer is at a lower address than the old,
        // then that's clearly incorrect. Treat this as end-of-stack to
        // enforce progress and avoid infinite loops.

        let sp = frame.context.get_stack_pointer();
        let last_sp = self.get_register_always(STACK_POINTER) as u64;
        if sp <= last_sp {
            // Mips leaf functions may not actually touch the stack (thanks
            // to the return address register allowing you to "push" the return address
            // to a register), so we need to permit the stack pointer to not
            // change for the first frame of the unwind. After that we need
            // more strict validation to avoid infinite loops.
            let is_leaf = callee.trust == FrameTrust::Context && sp == last_sp;
            if !is_leaf {
                trace!("stack pointer went backwards, assuming unwind complete");
                return None;
            }
        }

        // Ok, the frame now seems well and truly valid, do final cleanup.

        // The Mips `jal` instruction always sets $ra to PC + 8
        let ip = frame.context.get_instruction_pointer() as u64;
        frame.instruction = ip - 8;

        Some(frame)
    }
}

/// This is a hack to have a different [`CpuContext`] type/impl depending on the
/// context flags of the inner [`MipsContext`]
#[derive(Clone)]
struct Mips32Context(MipsContext);

impl CpuContext for Mips32Context {
    type Register = u32;

    const REGISTERS: &'static [&'static str] = <MipsContext as CpuContext>::REGISTERS;

    fn get_register_always(&self, reg: &str) -> Self::Register {
        self.0.get_register_always(reg) as u32
    }

    fn set_register(&mut self, reg: &str, val: Self::Register) -> Option<()> {
        self.0.set_register(reg, val.into())
    }

    fn stack_pointer_register_name(&self) -> &'static str {
        self.0.stack_pointer_register_name()
    }

    fn instruction_pointer_register_name(&self) -> &'static str {
        self.0.instruction_pointer_register_name()
    }
}

impl IntoRawContext for Mips32Context {
    fn into_ctx(self) -> MinidumpRawContext {
        MinidumpRawContext::Mips(self.0)
    }
}

trait IntoRawContext {
    fn into_ctx(self) -> MinidumpRawContext;
}

impl IntoRawContext for MipsContext {
    fn into_ctx(self) -> MinidumpRawContext {
        MinidumpRawContext::Mips(self)
    }
}

impl TryFrom<MipsContext> for Mips32Context {
    type Error = MipsContext;

    fn try_from(ctx: MipsContext) -> Result<Self, Self::Error> {
        if ContextFlagsCpu::from_flags(ctx.context_flags).contains(ContextFlagsCpu::CONTEXT_MIPS64)
        {
            Err(ctx)
        } else {
            Ok(Self(ctx))
        }
    }
}
