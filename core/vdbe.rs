use crate::btree::BTreeCursor;
use crate::function::{AggFunc, SingleRowFunc};
use crate::pager::Pager;
use crate::pseudo::PseudoCursor;
use crate::schema::Table;
use crate::types::{AggContext, Cursor, CursorResult, OwnedRecord, OwnedValue, Record};

use anyhow::Result;
use regex::Regex;
use std::borrow::BorrowMut;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

pub type BranchOffset = i64;

pub type CursorID = usize;

pub type PageIdx = usize;

#[derive(Debug)]
pub enum Insn {
    // Initialize the program state and jump to the given PC.
    Init {
        target_pc: BranchOffset,
    },
    // Set NULL in the given register.
    Null {
        dest: usize,
    },
    // Move the cursor P1 to a null row. Any Column operations that occur while the cursor is on the null row will always write a NULL.
    NullRow {
        cursor_id: CursorID,
    },
    // Add two registers and store the result in a third register.
    Add {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    // If the given register is a positive integer, decrement it by decrement_by and jump to the given PC.
    IfPos {
        reg: usize,
        target_pc: BranchOffset,
        decrement_by: usize,
    },
    // If the given register is not NULL, jump to the given PC.
    NotNull {
        reg: usize,
        target_pc: BranchOffset,
    },
    // Compare two registers and jump to the given PC if they are equal.
    Eq {
        lhs: usize,
        rhs: usize,
        target_pc: BranchOffset,
    },
    // Compare two registers and jump to the given PC if they are not equal.
    Ne {
        lhs: usize,
        rhs: usize,
        target_pc: BranchOffset,
    },
    // Compare two registers and jump to the given PC if the left-hand side is less than the right-hand side.
    Lt {
        lhs: usize,
        rhs: usize,
        target_pc: BranchOffset,
    },
    // Compare two registers and jump to the given PC if the left-hand side is less than or equal to the right-hand side.
    Le {
        lhs: usize,
        rhs: usize,
        target_pc: BranchOffset,
    },
    // Compare two registers and jump to the given PC if the left-hand side is greater than the right-hand side.
    Gt {
        lhs: usize,
        rhs: usize,
        target_pc: BranchOffset,
    },
    // Compare two registers and jump to the given PC if the left-hand side is greater than or equal to the right-hand side.
    Ge {
        lhs: usize,
        rhs: usize,
        target_pc: BranchOffset,
    },
    /// Jump to target_pc if r\[reg\] != 0 or (r\[reg\] == NULL && r\[null_reg\] != 0)
    If {
        reg: usize,              // P1
        target_pc: BranchOffset, // P2
        /// P3. If r\[reg\] is null, jump iff r\[null_reg\] != 0
        null_reg: usize,
    },
    /// Jump to target_pc if r\[reg\] != 0 or (r\[reg\] == NULL && r\[null_reg\] != 0)
    IfNot {
        reg: usize,              // P1
        target_pc: BranchOffset, // P2
        /// P3. If r\[reg\] is null, jump iff r\[null_reg\] != 0
        null_reg: usize,
    },
    // Open a cursor for reading.
    OpenReadAsync {
        cursor_id: CursorID,
        root_page: PageIdx,
    },

    // Await for the competion of open cursor.
    OpenReadAwait,

    // Open a cursor for a pseudo-table that contains a single row.
    OpenPseudo {
        cursor_id: CursorID,
        content_reg: usize,
        num_fields: usize,
    },

    // Rewind the cursor to the beginning of the B-Tree.
    RewindAsync {
        cursor_id: CursorID,
    },

    // Await for the completion of cursor rewind.
    RewindAwait {
        cursor_id: CursorID,
        pc_if_empty: BranchOffset,
    },

    // Read a column from the current row of the cursor.
    Column {
        cursor_id: CursorID,
        column: usize,
        dest: usize,
    },

    // Make a record and write it to destination register.
    MakeRecord {
        start_reg: usize, // P1
        count: usize,     // P2
        dest_reg: usize,  // P3
    },

    // Emit a row of results.
    ResultRow {
        start_reg: usize, // P1
        count: usize,     // P2
    },

    // Advance the cursor to the next row.
    NextAsync {
        cursor_id: CursorID,
    },

    // Await for the completion of cursor advance.
    NextAwait {
        cursor_id: CursorID,
        pc_if_next: BranchOffset,
    },

    // Halt the program.
    Halt,

    // Start a transaction.
    Transaction,

    // Branch to the given PC.
    Goto {
        target_pc: BranchOffset,
    },

    // Write an integer value into a register.
    Integer {
        value: i64,
        dest: usize,
    },

    // Write a float value into a register
    Real {
        value: f64,
        dest: usize,
    },

    // If register holds an integer, transform it to a float
    RealAffinity {
        register: usize,
    },

    // Write a string value into a register.
    String8 {
        value: String,
        dest: usize,
    },

    // Read the rowid of the current row.
    RowId {
        cursor_id: CursorID,
        dest: usize,
    },

    // Decrement the given register and jump to the given PC if the result is zero.
    DecrJumpZero {
        reg: usize,
        target_pc: BranchOffset,
    },

    AggStep {
        acc_reg: usize,
        col: usize,
        delimiter: usize,
        func: AggFunc,
    },

    AggFinal {
        register: usize,
        func: AggFunc,
    },

    // Open a sorter.
    SorterOpen {
        cursor_id: CursorID, // P1
        columns: usize,      // P2
        order: OwnedRecord,  // P4. 0 if ASC and 1 if DESC
    },

    // Insert a row into the sorter.
    SorterInsert {
        cursor_id: CursorID,
        record_reg: usize,
    },

    // Sort the rows in the sorter.
    SorterSort {
        cursor_id: CursorID,
        pc_if_empty: BranchOffset,
    },

    // Retrieve the next row from the sorter.
    SorterData {
        cursor_id: CursorID,  // P1
        dest_reg: usize,      // P2
        pseudo_cursor: usize, // P3
    },

    // Advance to the next row in the sorter.
    SorterNext {
        cursor_id: CursorID,
        pc_if_next: BranchOffset,
    },

    // Function
    Function {
        // constant_mask: i32, // P1, not used for now
        start_reg: usize,    // P2, start of argument registers
        dest: usize,         // P3
        func: SingleRowFunc, // P4
    },
}

// Index of insn in list of insns
type InsnReference = usize;

pub struct ProgramBuilder {
    next_free_register: usize,
    next_free_label: BranchOffset,
    next_free_cursor_id: usize,
    insns: Vec<Insn>,
    // for temporarily storing instructions that will be put after Transaction opcode
    constant_insns: Vec<Insn>,
    // Each label has a list of InsnReferences that must
    // be resolved. Lists are indexed by: label.abs() - 1
    unresolved_labels: Vec<Vec<InsnReference>>,
    next_insn_label: Option<BranchOffset>,
    // Cursors that are referenced by the program. Indexed by CursorID.
    cursor_ref: Vec<(Option<String>, Option<Table>)>,
    // List of deferred label resolutions. Each entry is a pair of (label, insn_reference).
    deferred_label_resolutions: Vec<(BranchOffset, InsnReference)>,
}

impl ProgramBuilder {
    pub fn new() -> Self {
        Self {
            next_free_register: 1,
            next_free_label: 0,
            next_free_cursor_id: 0,
            insns: Vec::new(),
            unresolved_labels: Vec::new(),
            next_insn_label: None,
            cursor_ref: Vec::new(),
            constant_insns: Vec::new(),
            deferred_label_resolutions: Vec::new(),
        }
    }

    pub fn alloc_register(&mut self) -> usize {
        let reg = self.next_free_register;
        self.next_free_register += 1;
        reg
    }

    pub fn alloc_registers(&mut self, amount: usize) -> usize {
        let reg = self.next_free_register;
        self.next_free_register += amount;
        reg
    }

    pub fn drop_register(&mut self) {
        self.next_free_register -= 1;
    }

    pub fn drop_registers(&mut self, amount: usize) {
        self.next_free_register -= amount;
    }

    pub fn next_free_register(&self) -> usize {
        self.next_free_register
    }

    pub fn alloc_cursor_id(
        &mut self,
        table_identifier: Option<String>,
        table: Option<Table>,
    ) -> usize {
        let cursor = self.next_free_cursor_id;
        self.next_free_cursor_id += 1;
        self.cursor_ref.push((table_identifier, table));
        assert!(self.cursor_ref.len() == self.next_free_cursor_id);
        cursor
    }

    pub fn emit_insn(&mut self, insn: Insn) {
        self.insns.push(insn);
        if let Some(label) = self.next_insn_label {
            self.next_insn_label = None;
            self.resolve_label(label, (self.insns.len() - 1) as BranchOffset);
        }
    }

    pub fn last_insn(&self) -> Option<&Insn> {
        self.insns.last()
    }

    pub fn last_of_type(&self, typ: std::mem::Discriminant<Insn>) -> Option<&Insn> {
        self.insns
            .iter()
            .rev()
            .find(|v| std::mem::discriminant(*v) == typ)
    }

    // Emit an instruction that will be put at the end of the program (after Transaction statement).
    // This is useful for instructions that otherwise will be unnecessarily repeated in a loop.
    // Example: In `SELECT * from users where name='John'`, it is unnecessary to set r[1]='John' as we SCAN users table.
    // We could simply set it once before the SCAN started.
    pub fn mark_last_insn_constant(&mut self) {
        self.constant_insns.push(self.insns.pop().unwrap());
    }

    pub fn emit_constant_insns(&mut self) {
        self.insns.append(&mut self.constant_insns);
    }

    pub fn emit_insn_with_label_dependency(&mut self, insn: Insn, label: BranchOffset) {
        self.insns.push(insn);
        self.add_label_dependency(label, (self.insns.len() - 1) as BranchOffset);
    }

    pub fn offset(&self) -> BranchOffset {
        self.insns.len() as BranchOffset
    }

    pub fn allocate_label(&mut self) -> BranchOffset {
        self.next_free_label -= 1;
        self.unresolved_labels.push(Vec::new());
        self.next_free_label
    }

    // Effectively a GOTO <next insn> without the need to emit an explicit GOTO instruction.
    // Useful when you know you need to jump to "the next part", but the exact offset is unknowable
    // at the time of emitting the instruction.
    pub fn preassign_label_to_next_insn(&mut self, label: BranchOffset) {
        self.next_insn_label = Some(label);
    }

    fn label_to_index(&self, label: BranchOffset) -> usize {
        (label.abs() - 1) as usize
    }

    pub fn add_label_dependency(&mut self, label: BranchOffset, insn_reference: BranchOffset) {
        assert!(insn_reference >= 0);
        assert!(label < 0);
        let label_index = self.label_to_index(label);
        assert!(label_index < self.unresolved_labels.len());
        let insn_reference = insn_reference as InsnReference;
        let label_references = &mut self.unresolved_labels[label_index];
        label_references.push(insn_reference);
    }

    pub fn defer_label_resolution(&mut self, label: BranchOffset, insn_reference: InsnReference) {
        self.deferred_label_resolutions
            .push((label, insn_reference));
    }

    pub fn resolve_label(&mut self, label: BranchOffset, to_offset: BranchOffset) {
        assert!(label < 0);
        assert!(to_offset >= 0);
        let label_index = self.label_to_index(label);
        assert!(
            label_index < self.unresolved_labels.len(),
            "Forbidden resolve of an unexistent label!"
        );

        let label_references = &mut self.unresolved_labels[label_index];
        for insn_reference in label_references.iter() {
            let insn = &mut self.insns[*insn_reference];
            match insn {
                Insn::Init { target_pc } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                Insn::Eq {
                    lhs: _lhs,
                    rhs: _rhs,
                    target_pc,
                } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                Insn::Ne {
                    lhs: _lhs,
                    rhs: _rhs,
                    target_pc,
                } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                Insn::Lt {
                    lhs: _lhs,
                    rhs: _rhs,
                    target_pc,
                } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                Insn::Le {
                    lhs: _lhs,
                    rhs: _rhs,
                    target_pc,
                } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                Insn::Gt {
                    lhs: _lhs,
                    rhs: _rhs,
                    target_pc,
                } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                Insn::Ge {
                    lhs: _lhs,
                    rhs: _rhs,
                    target_pc,
                } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                Insn::If {
                    reg: _reg,
                    target_pc,
                    null_reg,
                } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                Insn::IfNot {
                    reg: _reg,
                    target_pc,
                    null_reg,
                } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                Insn::RewindAwait {
                    cursor_id: _cursor_id,
                    pc_if_empty,
                } => {
                    assert!(*pc_if_empty < 0);
                    *pc_if_empty = to_offset;
                }
                Insn::Goto { target_pc } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                Insn::DecrJumpZero {
                    reg: _reg,
                    target_pc,
                } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                Insn::SorterNext {
                    cursor_id: _cursor_id,
                    pc_if_next,
                } => {
                    assert!(*pc_if_next < 0);
                    *pc_if_next = to_offset;
                }
                Insn::SorterSort { pc_if_empty, .. } => {
                    assert!(*pc_if_empty < 0);
                    *pc_if_empty = to_offset;
                }
                Insn::NotNull {
                    reg: _reg,
                    target_pc,
                } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                Insn::IfPos { target_pc, .. } => {
                    assert!(*target_pc < 0);
                    *target_pc = to_offset;
                }
                _ => {
                    todo!("missing resolve_label for {:?}", insn);
                }
            }
        }
        label_references.clear();
    }

    // translate table to cursor id
    pub fn resolve_cursor_id(
        &self,
        table_identifier: &str,
        cursor_hint: Option<CursorID>,
    ) -> CursorID {
        if let Some(cursor_hint) = cursor_hint {
            return cursor_hint;
        }
        self.cursor_ref
            .iter()
            .position(|(t_ident, _)| {
                t_ident
                    .as_ref()
                    .is_some_and(|ident| ident == table_identifier)
            })
            .unwrap()
    }

    pub fn resolve_deferred_labels(&mut self) {
        for i in 0..self.deferred_label_resolutions.len() {
            let (label, insn_reference) = self.deferred_label_resolutions[i];
            self.resolve_label(label, insn_reference as BranchOffset);
        }
        self.deferred_label_resolutions.clear();
    }

    pub fn build(self) -> Program {
        assert!(
            self.deferred_label_resolutions.is_empty(),
            "deferred_label_resolutions is not empty when build() is called, did you forget to call resolve_deferred_labels()?"
        );
        assert!(
            self.constant_insns.is_empty(),
            "constant_insns is not empty when build() is called, did you forget to call emit_constant_insns()?"
        );
        Program {
            max_registers: self.next_free_register,
            insns: self.insns,
            cursor_ref: self.cursor_ref,
        }
    }
}

pub enum StepResult<'a> {
    Done,
    IO,
    Row(Record<'a>),
}

/// The program state describes the environment in which the program executes.
pub struct ProgramState {
    pub pc: BranchOffset,
    cursors: RefCell<BTreeMap<CursorID, Box<dyn Cursor>>>,
    registers: Vec<OwnedValue>,
}

impl ProgramState {
    pub fn new(max_registers: usize) -> Self {
        let cursors = RefCell::new(BTreeMap::new());
        let mut registers = Vec::with_capacity(max_registers);
        registers.resize(max_registers, OwnedValue::Null);
        Self {
            pc: 0,
            cursors,
            registers,
        }
    }

    pub fn column_count(&self) -> usize {
        self.registers.len()
    }

    pub fn column(&self, i: usize) -> Option<String> {
        Some(format!("{:?}", self.registers[i]))
    }
}

pub struct Program {
    pub max_registers: usize,
    pub insns: Vec<Insn>,
    pub cursor_ref: Vec<(Option<String>, Option<Table>)>,
}

impl Program {
    pub fn explain(&self) {
        println!("addr  opcode             p1    p2    p3    p4             p5  comment");
        println!("----  -----------------  ----  ----  ----  -------------  --  -------");
        let mut indent_count: usize = 0;
        let indent = "  ";
        let mut prev_insn: Option<&Insn> = None;
        for (addr, insn) in self.insns.iter().enumerate() {
            indent_count = get_indent_count(indent_count, insn, prev_insn);
            print_insn(
                self,
                addr as InsnReference,
                insn,
                indent.repeat(indent_count),
            );
            prev_insn = Some(insn);
        }
    }

    pub fn step<'a>(
        &self,
        state: &'a mut ProgramState,
        pager: Rc<Pager>,
    ) -> Result<StepResult<'a>> {
        loop {
            let insn = &self.insns[state.pc as usize];
            trace_insn(self, state.pc as InsnReference, insn);
            let mut cursors = state.cursors.borrow_mut();
            match insn {
                Insn::Init { target_pc } => {
                    assert!(*target_pc >= 0);
                    state.pc = *target_pc;
                }
                Insn::Add { lhs, rhs, dest } => {
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let dest = *dest;
                    match (&state.registers[lhs], &state.registers[rhs]) {
                        (OwnedValue::Integer(lhs), OwnedValue::Integer(rhs)) => {
                            state.registers[dest] = OwnedValue::Integer(lhs + rhs);
                        }
                        (OwnedValue::Float(lhs), OwnedValue::Float(rhs)) => {
                            state.registers[dest] = OwnedValue::Float(lhs + rhs);
                        }
                        _ => {
                            todo!();
                        }
                    }
                    state.pc += 1;
                }
                Insn::Null { dest } => {
                    state.registers[*dest] = OwnedValue::Null;
                    state.pc += 1;
                }
                Insn::NullRow { cursor_id } => {
                    let cursor = cursors.get_mut(cursor_id).unwrap();
                    cursor.set_null_flag(true);
                    state.pc += 1;
                }
                Insn::IfPos {
                    reg,
                    target_pc,
                    decrement_by,
                } => {
                    assert!(*target_pc >= 0);
                    let reg = *reg;
                    let target_pc = *target_pc;
                    match &state.registers[reg] {
                        OwnedValue::Integer(n) if *n > 0 => {
                            state.pc = target_pc;
                            state.registers[reg] = OwnedValue::Integer(*n - *decrement_by as i64);
                        }
                        OwnedValue::Integer(_) => {
                            state.pc += 1;
                        }
                        _ => {
                            anyhow::bail!("IfPos: the value in the register is not an integer");
                        }
                    }
                }
                Insn::NotNull { reg, target_pc } => {
                    assert!(*target_pc >= 0);
                    let reg = *reg;
                    let target_pc = *target_pc;
                    match &state.registers[reg] {
                        OwnedValue::Null => {
                            state.pc += 1;
                        }
                        _ => {
                            state.pc = target_pc;
                        }
                    }
                }

                Insn::Eq {
                    lhs,
                    rhs,
                    target_pc,
                } => {
                    assert!(*target_pc >= 0);
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let target_pc = *target_pc;
                    match (&state.registers[lhs], &state.registers[rhs]) {
                        (_, OwnedValue::Null) | (OwnedValue::Null, _) => {
                            state.pc = target_pc;
                        }
                        _ => {
                            if &state.registers[lhs] == &state.registers[rhs] {
                                state.pc = target_pc;
                            } else {
                                state.pc += 1;
                            }
                        }
                    }
                }
                Insn::Ne {
                    lhs,
                    rhs,
                    target_pc,
                } => {
                    assert!(*target_pc >= 0);
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let target_pc = *target_pc;
                    match (&state.registers[lhs], &state.registers[rhs]) {
                        (_, OwnedValue::Null) | (OwnedValue::Null, _) => {
                            state.pc = target_pc;
                        }
                        _ => {
                            if &state.registers[lhs] != &state.registers[rhs] {
                                state.pc = target_pc;
                            } else {
                                state.pc += 1;
                            }
                        }
                    }
                }
                Insn::Lt {
                    lhs,
                    rhs,
                    target_pc,
                } => {
                    assert!(*target_pc >= 0);
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let target_pc = *target_pc;
                    match (&state.registers[lhs], &state.registers[rhs]) {
                        (_, OwnedValue::Null) | (OwnedValue::Null, _) => {
                            state.pc = target_pc;
                        }
                        _ => {
                            if &state.registers[lhs] < &state.registers[rhs] {
                                state.pc = target_pc;
                            } else {
                                state.pc += 1;
                            }
                        }
                    }
                }
                Insn::Le {
                    lhs,
                    rhs,
                    target_pc,
                } => {
                    assert!(*target_pc >= 0);
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let target_pc = *target_pc;
                    match (&state.registers[lhs], &state.registers[rhs]) {
                        (_, OwnedValue::Null) | (OwnedValue::Null, _) => {
                            state.pc = target_pc;
                        }
                        _ => {
                            if &state.registers[lhs] <= &state.registers[rhs] {
                                state.pc = target_pc;
                            } else {
                                state.pc += 1;
                            }
                        }
                    }
                }
                Insn::Gt {
                    lhs,
                    rhs,
                    target_pc,
                } => {
                    assert!(*target_pc >= 0);
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let target_pc = *target_pc;
                    match (&state.registers[lhs], &state.registers[rhs]) {
                        (_, OwnedValue::Null) | (OwnedValue::Null, _) => {
                            state.pc = target_pc;
                        }
                        _ => {
                            if &state.registers[lhs] > &state.registers[rhs] {
                                state.pc = target_pc;
                            } else {
                                state.pc += 1;
                            }
                        }
                    }
                }
                Insn::Ge {
                    lhs,
                    rhs,
                    target_pc,
                } => {
                    assert!(*target_pc >= 0);
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let target_pc = *target_pc;
                    match (&state.registers[lhs], &state.registers[rhs]) {
                        (_, OwnedValue::Null) | (OwnedValue::Null, _) => {
                            state.pc = target_pc;
                        }
                        _ => {
                            if &state.registers[lhs] >= &state.registers[rhs] {
                                state.pc = target_pc;
                            } else {
                                state.pc += 1;
                            }
                        }
                    }
                }
                Insn::If {
                    reg,
                    target_pc,
                    null_reg,
                } => {
                    assert!(*target_pc >= 0);
                    if exec_if(&state.registers[*reg], &state.registers[*null_reg], false) {
                        state.pc = *target_pc;
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::IfNot {
                    reg,
                    target_pc,
                    null_reg,
                } => {
                    assert!(*target_pc >= 0);
                    if exec_if(&state.registers[*reg], &state.registers[*null_reg], true) {
                        state.pc = *target_pc;
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::OpenReadAsync {
                    cursor_id,
                    root_page,
                } => {
                    let cursor = Box::new(BTreeCursor::new(pager.clone(), *root_page));
                    cursors.insert(*cursor_id, cursor);
                    state.pc += 1;
                }
                Insn::OpenReadAwait => {
                    state.pc += 1;
                }
                Insn::OpenPseudo {
                    cursor_id,
                    content_reg: _,
                    num_fields: _,
                } => {
                    let cursor = Box::new(PseudoCursor::new());
                    cursors.insert(*cursor_id, cursor);
                    state.pc += 1;
                }
                Insn::RewindAsync { cursor_id } => {
                    let cursor = cursors.get_mut(cursor_id).unwrap();
                    match cursor.rewind()? {
                        CursorResult::Ok(()) => {}
                        CursorResult::IO => {
                            // If there is I/O, the instruction is restarted.
                            return Ok(StepResult::IO);
                        }
                    }
                    state.pc += 1;
                }
                Insn::RewindAwait {
                    cursor_id,
                    pc_if_empty,
                } => {
                    let cursor = cursors.get_mut(cursor_id).unwrap();
                    cursor.wait_for_completion()?;
                    if cursor.is_empty() {
                        state.pc = *pc_if_empty;
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::Column {
                    cursor_id,
                    column,
                    dest,
                } => {
                    let cursor = cursors.get_mut(cursor_id).unwrap();
                    if let Some(ref record) = cursor.record()? {
                        let null_flag = cursor.get_null_flag();
                        state.registers[*dest] = if null_flag {
                            OwnedValue::Null
                        } else {
                            record.values[*column].clone()
                        };
                    } else {
                        state.registers[*dest] = OwnedValue::Null;
                    }
                    state.pc += 1;
                }
                Insn::MakeRecord {
                    start_reg,
                    count,
                    dest_reg,
                } => {
                    let record = make_owned_record(&state.registers, start_reg, count);
                    state.registers[*dest_reg] = OwnedValue::Record(record);
                    state.pc += 1;
                }
                Insn::ResultRow { start_reg, count } => {
                    let record = make_record(&state.registers, start_reg, count);
                    state.pc += 1;
                    return Ok(StepResult::Row(record));
                }
                Insn::NextAsync { cursor_id } => {
                    let cursor = cursors.get_mut(cursor_id).unwrap();
                    cursor.set_null_flag(false);
                    match cursor.next()? {
                        CursorResult::Ok(_) => {}
                        CursorResult::IO => {
                            // If there is I/O, the instruction is restarted.
                            return Ok(StepResult::IO);
                        }
                    }
                    state.pc += 1;
                }
                Insn::NextAwait {
                    cursor_id,
                    pc_if_next,
                } => {
                    assert!(*pc_if_next >= 0);
                    let cursor = cursors.get_mut(cursor_id).unwrap();
                    cursor.wait_for_completion()?;
                    if !cursor.is_empty() {
                        state.pc = *pc_if_next;
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::Halt => {
                    return Ok(StepResult::Done);
                }
                Insn::Transaction => {
                    state.pc += 1;
                }
                Insn::Goto { target_pc } => {
                    assert!(*target_pc >= 0);
                    state.pc = *target_pc;
                }
                Insn::Integer { value, dest } => {
                    state.registers[*dest] = OwnedValue::Integer(*value);
                    state.pc += 1;
                }
                Insn::Real { value, dest } => {
                    state.registers[*dest] = OwnedValue::Float(*value);
                    state.pc += 1;
                }
                Insn::RealAffinity { register } => {
                    if let OwnedValue::Integer(i) = &state.registers[*register] {
                        state.registers[*register] = OwnedValue::Float(*i as f64);
                    };
                    state.pc += 1;
                }
                Insn::String8 { value, dest } => {
                    state.registers[*dest] = OwnedValue::Text(Rc::new(value.into()));
                    state.pc += 1;
                }
                Insn::RowId { cursor_id, dest } => {
                    let cursor = cursors.get_mut(cursor_id).unwrap();
                    if let Some(ref rowid) = cursor.rowid()? {
                        state.registers[*dest] = OwnedValue::Integer(*rowid as i64);
                    } else {
                        todo!();
                    }
                    state.pc += 1;
                }
                Insn::DecrJumpZero { reg, target_pc } => {
                    assert!(*target_pc >= 0);
                    match state.registers[*reg] {
                        OwnedValue::Integer(n) => {
                            let n = n - 1;
                            if n == 0 {
                                state.pc = *target_pc;
                            } else {
                                state.registers[*reg] = OwnedValue::Integer(n);
                                state.pc += 1;
                            }
                        }
                        _ => unreachable!("DecrJumpZero on non-integer register"),
                    }
                }
                Insn::AggStep {
                    acc_reg,
                    col,
                    delimiter,
                    func,
                } => {
                    if let OwnedValue::Null = &state.registers[*acc_reg] {
                        state.registers[*acc_reg] = match func {
                            AggFunc::Avg => OwnedValue::Agg(Box::new(AggContext::Avg(
                                OwnedValue::Float(0.0),
                                OwnedValue::Integer(0),
                            ))),
                            AggFunc::Sum => {
                                OwnedValue::Agg(Box::new(AggContext::Sum(OwnedValue::Null)))
                            }
                            AggFunc::Total => {
                                // The result of total() is always a floating point value.
                                // No overflow error is ever raised if any prior input was a floating point value.
                                // Total() never throws an integer overflow.
                                OwnedValue::Agg(Box::new(AggContext::Sum(OwnedValue::Float(0.0))))
                            }
                            AggFunc::Count => {
                                OwnedValue::Agg(Box::new(AggContext::Count(OwnedValue::Integer(0))))
                            }
                            AggFunc::Max => {
                                let col = state.registers[*col].clone();
                                match col {
                                    OwnedValue::Integer(_) => OwnedValue::Agg(Box::new(
                                        AggContext::Max(OwnedValue::Integer(i64::MIN)),
                                    )),
                                    OwnedValue::Float(_) => OwnedValue::Agg(Box::new(
                                        AggContext::Max(OwnedValue::Float(f64::NEG_INFINITY)),
                                    )),
                                    _ => {
                                        unreachable!();
                                    }
                                }
                            }
                            AggFunc::Min => {
                                let col = state.registers[*col].clone();
                                match col {
                                    OwnedValue::Integer(_) => OwnedValue::Agg(Box::new(
                                        AggContext::Min(OwnedValue::Integer(i64::MAX)),
                                    )),
                                    OwnedValue::Float(_) => OwnedValue::Agg(Box::new(
                                        AggContext::Min(OwnedValue::Float(f64::INFINITY)),
                                    )),
                                    _ => {
                                        unreachable!();
                                    }
                                }
                            }
                            AggFunc::GroupConcat | AggFunc::StringAgg => OwnedValue::Agg(Box::new(
                                AggContext::GroupConcat(OwnedValue::Text(Rc::new("".to_string()))),
                            )),
                        };
                    }
                    match func {
                        AggFunc::Avg => {
                            let col = state.registers[*col].clone();
                            let OwnedValue::Agg(agg) = state.registers[*acc_reg].borrow_mut()
                            else {
                                unreachable!();
                            };
                            let AggContext::Avg(acc, count) = agg.borrow_mut() else {
                                unreachable!();
                            };
                            *acc += col;
                            *count += 1;
                        }
                        AggFunc::Sum | AggFunc::Total => {
                            let col = state.registers[*col].clone();
                            let OwnedValue::Agg(agg) = state.registers[*acc_reg].borrow_mut()
                            else {
                                unreachable!();
                            };
                            let AggContext::Sum(acc) = agg.borrow_mut() else {
                                unreachable!();
                            };
                            *acc += col;
                        }
                        AggFunc::Count => {
                            let OwnedValue::Agg(agg) = state.registers[*acc_reg].borrow_mut()
                            else {
                                unreachable!();
                            };
                            let AggContext::Count(count) = agg.borrow_mut() else {
                                unreachable!();
                            };
                            *count += 1;
                        }
                        AggFunc::Max => {
                            let col = state.registers[*col].clone();
                            let OwnedValue::Agg(agg) = state.registers[*acc_reg].borrow_mut()
                            else {
                                unreachable!();
                            };
                            let AggContext::Max(acc) = agg.borrow_mut() else {
                                unreachable!();
                            };

                            match (acc, col) {
                                (
                                    OwnedValue::Integer(ref mut current_max),
                                    OwnedValue::Integer(value),
                                ) => {
                                    if value > *current_max {
                                        *current_max = value;
                                    }
                                }
                                (
                                    OwnedValue::Float(ref mut current_max),
                                    OwnedValue::Float(value),
                                ) => {
                                    if value > *current_max {
                                        *current_max = value;
                                    }
                                }
                                _ => {
                                    eprintln!("Unexpected types in max aggregation");
                                }
                            }
                        }
                        AggFunc::Min => {
                            let col = state.registers[*col].clone();
                            let OwnedValue::Agg(agg) = state.registers[*acc_reg].borrow_mut()
                            else {
                                unreachable!();
                            };
                            let AggContext::Min(acc) = agg.borrow_mut() else {
                                unreachable!();
                            };

                            match (acc, col) {
                                (
                                    OwnedValue::Integer(ref mut current_min),
                                    OwnedValue::Integer(value),
                                ) => {
                                    if value < *current_min {
                                        *current_min = value;
                                    }
                                }
                                (
                                    OwnedValue::Float(ref mut current_min),
                                    OwnedValue::Float(value),
                                ) => {
                                    if value < *current_min {
                                        *current_min = value;
                                    }
                                }
                                _ => {
                                    eprintln!("Unexpected types in min aggregation");
                                }
                            }
                        }
                        AggFunc::GroupConcat | AggFunc::StringAgg => {
                            let col = state.registers[*col].clone();
                            let delimiter = state.registers[*delimiter].clone();
                            let OwnedValue::Agg(agg) = state.registers[*acc_reg].borrow_mut()
                            else {
                                unreachable!();
                            };
                            let AggContext::GroupConcat(acc) = agg.borrow_mut() else {
                                unreachable!();
                            };
                            if acc.to_string().is_empty() {
                                *acc = col;
                            } else {
                                *acc += delimiter;
                                *acc += col;
                            }
                        }
                    };
                    state.pc += 1;
                }
                Insn::AggFinal { register, func } => {
                    match state.registers[*register].borrow_mut() {
                        OwnedValue::Agg(agg) => {
                            match func {
                                AggFunc::Avg => {
                                    let AggContext::Avg(acc, count) = agg.borrow_mut() else {
                                        unreachable!();
                                    };
                                    *acc /= count.clone();
                                }
                                AggFunc::Sum | AggFunc::Total => {}
                                AggFunc::Count => {}
                                AggFunc::Max => {}
                                AggFunc::Min => {}
                                AggFunc::GroupConcat | AggFunc::StringAgg => {}
                            };
                        }
                        OwnedValue::Null => {
                            // when the set is empty
                            match func {
                                AggFunc::Total => {
                                    state.registers[*register] = OwnedValue::Float(0.0);
                                }
                                AggFunc::Count => {
                                    state.registers[*register] = OwnedValue::Integer(0);
                                }
                                _ => {}
                            }
                        }
                        _ => {
                            unreachable!();
                        }
                    };
                    state.pc += 1;
                }
                Insn::SorterOpen {
                    cursor_id,
                    columns,
                    order,
                } => {
                    let order = order
                        .values
                        .iter()
                        .map(|v| match v {
                            OwnedValue::Integer(i) => *i == 0,
                            _ => unreachable!(),
                        })
                        .collect();
                    let cursor = Box::new(crate::sorter::Sorter::new(order));
                    cursors.insert(*cursor_id, cursor);
                    state.pc += 1;
                }
                Insn::SorterData {
                    cursor_id,
                    dest_reg,
                    pseudo_cursor: sorter_cursor,
                } => {
                    let cursor = cursors.get_mut(cursor_id).unwrap();
                    let record = match cursor.record()? {
                        Some(ref record) => record.clone(),
                        None => {
                            todo!();
                        }
                    };
                    state.registers[*dest_reg] = OwnedValue::Record(record.clone());
                    let sorter_cursor = cursors.get_mut(sorter_cursor).unwrap();
                    sorter_cursor.insert(&record)?;
                    state.pc += 1;
                }
                Insn::SorterInsert {
                    cursor_id,
                    record_reg,
                } => {
                    let cursor = cursors.get_mut(cursor_id).unwrap();
                    let record = match &state.registers[*record_reg] {
                        OwnedValue::Record(record) => record,
                        _ => unreachable!("SorterInsert on non-record register"),
                    };
                    cursor.insert(record)?;
                    state.pc += 1;
                }
                Insn::SorterSort {
                    cursor_id,
                    pc_if_empty,
                } => {
                    if let Some(cursor) = cursors.get_mut(cursor_id) {
                        cursor.rewind()?;
                        state.pc += 1;
                    } else {
                        state.pc = *pc_if_empty;
                    }
                }
                Insn::SorterNext {
                    cursor_id,
                    pc_if_next,
                } => {
                    assert!(*pc_if_next >= 0);
                    let cursor = cursors.get_mut(cursor_id).unwrap();
                    match cursor.next()? {
                        CursorResult::Ok(_) => {}
                        CursorResult::IO => {
                            // If there is I/O, the instruction is restarted.
                            return Ok(StepResult::IO);
                        }
                    }
                    if !cursor.is_empty() {
                        state.pc = *pc_if_next;
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::Function {
                    func,
                    start_reg,
                    dest,
                } => match func {
                    SingleRowFunc::Coalesce => {}
                    SingleRowFunc::Like => {
                        let start_reg = *start_reg;
                        assert!(
                            start_reg + 2 <= state.registers.len(),
                            "not enough registers {} < {}",
                            start_reg,
                            state.registers.len()
                        );
                        let pattern = state.registers[start_reg].clone();
                        let text = state.registers[start_reg + 1].clone();
                        let result = match (pattern, text) {
                            (OwnedValue::Text(pattern), OwnedValue::Text(text)) => {
                                OwnedValue::Integer(exec_like(&pattern, &text) as i64)
                            }
                            _ => {
                                unreachable!("Like on non-text registers");
                            }
                        };
                        state.registers[*dest] = result;
                        state.pc += 1;
                    }
                    SingleRowFunc::Abs => {
                        let reg_value = state.registers[*start_reg].borrow_mut();
                        if let Some(value) = exec_abs(reg_value) {
                            state.registers[*dest] = value;
                        } else {
                            state.registers[*dest] = OwnedValue::Null;
                        }
                        state.pc += 1;
                    }
                    SingleRowFunc::Upper => {
                        let reg_value = state.registers[*start_reg].borrow_mut();
                        if let Some(value) = exec_upper(reg_value) {
                            state.registers[*dest] = value;
                        } else {
                            state.registers[*dest] = OwnedValue::Null;
                        }
                        state.pc += 1;
                    }
                    SingleRowFunc::Lower => {
                        let reg_value = state.registers[*start_reg].borrow_mut();
                        if let Some(value) = exec_lower(reg_value) {
                            state.registers[*dest] = value;
                        } else {
                            state.registers[*dest] = OwnedValue::Null;
                        }
                        state.pc += 1;
                    }
                    SingleRowFunc::Length => {
                        let reg_value = state.registers[*start_reg].borrow_mut();
                        state.registers[*dest] = exec_length(reg_value);
                        state.pc += 1;
                    }
                    SingleRowFunc::Random => {
                        state.registers[*dest] = exec_random();
                        state.pc += 1;
                    }
                    SingleRowFunc::Trim => {
                        let start_reg = *start_reg;
                        let reg_value = state.registers[start_reg].clone();
                        let pattern_value = state.registers.get(start_reg + 1).cloned();

                        let result = exec_trim(&reg_value, pattern_value);

                        state.registers[*dest] = result;
                        state.pc += 1;
                    }
                    SingleRowFunc::Round => {
                        let start_reg = *start_reg;
                        let reg_value = state.registers[start_reg].clone();
                        let precision_value = state.registers.get(start_reg + 1).cloned();
                        let result = exec_round(&reg_value, precision_value);
                        state.registers[*dest] = result;
                        state.pc += 1;
                    }
                },
            }
        }
    }
}

fn make_record<'a>(registers: &'a [OwnedValue], start_reg: &usize, count: &usize) -> Record<'a> {
    let mut values = Vec::with_capacity(*count);
    for r in registers.iter().skip(*start_reg).take(*count) {
        values.push(crate::types::to_value(r))
    }
    Record::new(values)
}

fn make_owned_record(registers: &[OwnedValue], start_reg: &usize, count: &usize) -> OwnedRecord {
    let mut values = Vec::with_capacity(*count);
    for r in registers.iter().skip(*start_reg).take(*count) {
        values.push(r.clone())
    }
    OwnedRecord::new(values)
}

fn trace_insn(program: &Program, addr: InsnReference, insn: &Insn) {
    if !log::log_enabled!(log::Level::Trace) {
        return;
    }
    log::trace!("{}", insn_to_str(program, addr, insn, String::new()));
}

fn print_insn(program: &Program, addr: InsnReference, insn: &Insn, indent: String) {
    let s = insn_to_str(program, addr, insn, indent);
    println!("{}", s);
}

fn insn_to_str(program: &Program, addr: InsnReference, insn: &Insn, indent: String) -> String {
    let (opcode, p1, p2, p3, p4, p5, comment): (&str, i32, i32, i32, OwnedValue, u16, String) =
        match insn {
            Insn::Init { target_pc } => (
                "Init",
                0,
                *target_pc as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("Start at {}", target_pc),
            ),
            Insn::Add { lhs, rhs, dest } => (
                "Add",
                *lhs as i32,
                *rhs as i32,
                *dest as i32,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("r[{}]=r[{}]+r[{}]", dest, lhs, rhs),
            ),
            Insn::Null { dest } => (
                "Null",
                *dest as i32,
                0,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("r[{}]=NULL", dest),
            ),
            Insn::NullRow { cursor_id } => (
                "NullRow",
                *cursor_id as i32,
                0,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("Set cursor {} to a (pseudo) NULL row", cursor_id),
            ),
            Insn::NotNull { reg, target_pc } => (
                "NotNull",
                *reg as i32,
                *target_pc as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("r[{}] -> {}", reg, target_pc),
            ),
            Insn::IfPos {
                reg,
                target_pc,
                decrement_by,
            } => (
                "IfPos",
                *reg as i32,
                *target_pc as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!(
                    "r[{}]>0 -> r[{}]-={}, goto {}",
                    reg, reg, decrement_by, target_pc
                ),
            ),
            Insn::Eq {
                lhs,
                rhs,
                target_pc,
            } => (
                "Eq",
                *lhs as i32,
                *rhs as i32,
                *target_pc as i32,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("if r[{}]==r[{}] goto {}", lhs, rhs, target_pc),
            ),
            Insn::Ne {
                lhs,
                rhs,
                target_pc,
            } => (
                "Ne",
                *lhs as i32,
                *rhs as i32,
                *target_pc as i32,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("if r[{}]!=r[{}] goto {}", lhs, rhs, target_pc),
            ),
            Insn::Lt {
                lhs,
                rhs,
                target_pc,
            } => (
                "Lt",
                *lhs as i32,
                *rhs as i32,
                *target_pc as i32,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("if r[{}]<r[{}] goto {}", lhs, rhs, target_pc),
            ),
            Insn::Le {
                lhs,
                rhs,
                target_pc,
            } => (
                "Le",
                *lhs as i32,
                *rhs as i32,
                *target_pc as i32,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("if r[{}]<=r[{}] goto {}", lhs, rhs, target_pc),
            ),
            Insn::Gt {
                lhs,
                rhs,
                target_pc,
            } => (
                "Gt",
                *lhs as i32,
                *rhs as i32,
                *target_pc as i32,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("if r[{}]>r[{}] goto {}", lhs, rhs, target_pc),
            ),
            Insn::Ge {
                lhs,
                rhs,
                target_pc,
            } => (
                "Ge",
                *lhs as i32,
                *rhs as i32,
                *target_pc as i32,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("if r[{}]>=r[{}] goto {}", lhs, rhs, target_pc),
            ),
            Insn::If {
                reg,
                target_pc,
                null_reg,
            } => (
                "If",
                *reg as i32,
                *target_pc as i32,
                *null_reg as i32,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("if r[{}] goto {}", reg, target_pc),
            ),
            Insn::IfNot {
                reg,
                target_pc,
                null_reg,
            } => (
                "IfNot",
                *reg as i32,
                *target_pc as i32,
                *null_reg as i32,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("if !r[{}] goto {}", reg, target_pc),
            ),
            Insn::OpenReadAsync {
                cursor_id,
                root_page,
            } => (
                "OpenReadAsync",
                *cursor_id as i32,
                *root_page as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("root={}", root_page),
            ),
            Insn::OpenReadAwait => (
                "OpenReadAwait",
                0,
                0,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                "".to_string(),
            ),
            Insn::OpenPseudo {
                cursor_id,
                content_reg,
                num_fields,
            } => (
                "OpenPseudo",
                *cursor_id as i32,
                *content_reg as i32,
                *num_fields as i32,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("{} columns in r[{}]", num_fields, content_reg),
            ),
            Insn::RewindAsync { cursor_id } => (
                "RewindAsync",
                *cursor_id as i32,
                0,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                "".to_string(),
            ),
            Insn::RewindAwait {
                cursor_id,
                pc_if_empty,
            } => (
                "RewindAwait",
                *cursor_id as i32,
                *pc_if_empty as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                "".to_string(),
            ),
            Insn::Column {
                cursor_id,
                column,
                dest,
            } => {
                let (_, table) = &program.cursor_ref[*cursor_id];
                (
                    "Column",
                    *cursor_id as i32,
                    *column as i32,
                    *dest as i32,
                    OwnedValue::Text(Rc::new("".to_string())),
                    0,
                    format!(
                        "r[{}]={}.{}",
                        dest,
                        table
                            .as_ref()
                            .and_then(|x| Some(x.get_name()))
                            .unwrap_or(format!("cursor {}", cursor_id).as_str()),
                        table
                            .as_ref()
                            .and_then(|x| x.column_index_to_name(*column))
                            .unwrap_or(format!("column {}", *column).as_str())
                    ),
                )
            }
            Insn::MakeRecord {
                start_reg,
                count,
                dest_reg,
            } => (
                "MakeRecord",
                *start_reg as i32,
                *count as i32,
                *dest_reg as i32,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!(
                    "r[{}]=mkrec(r[{}..{}])",
                    dest_reg,
                    start_reg,
                    start_reg + count - 1,
                ),
            ),
            Insn::ResultRow { start_reg, count } => (
                "ResultRow",
                *start_reg as i32,
                *count as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                if *count == 1 {
                    format!("output=r[{}]", start_reg)
                } else {
                    format!("output=r[{}..{}]", start_reg, start_reg + count - 1)
                },
            ),
            Insn::NextAsync { cursor_id } => (
                "NextAsync",
                *cursor_id as i32,
                0,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                "".to_string(),
            ),
            Insn::NextAwait {
                cursor_id,
                pc_if_next,
            } => (
                "NextAwait",
                *cursor_id as i32,
                *pc_if_next as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                "".to_string(),
            ),
            Insn::Halt => (
                "Halt",
                0,
                0,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                "".to_string(),
            ),
            Insn::Transaction => (
                "Transaction",
                0,
                0,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                "".to_string(),
            ),
            Insn::Goto { target_pc } => (
                "Goto",
                0,
                *target_pc as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                "".to_string(),
            ),
            Insn::Integer { value, dest } => (
                "Integer",
                *value as i32,
                *dest as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("r[{}]={}", dest, value),
            ),
            Insn::Real { value, dest } => (
                "Real",
                0,
                *dest as i32,
                0,
                OwnedValue::Float(*value),
                0,
                format!("r[{}]={}", dest, value),
            ),
            Insn::RealAffinity { register } => (
                "RealAffinity",
                *register as i32,
                0,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                "".to_string(),
            ),
            Insn::String8 { value, dest } => (
                "String8",
                0,
                *dest as i32,
                0,
                OwnedValue::Text(Rc::new(value.clone())),
                0,
                format!("r[{}]='{}'", dest, value),
            ),
            Insn::RowId { cursor_id, dest } => (
                "RowId",
                *cursor_id as i32,
                *dest as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!(
                    "r[{}]={}.rowid",
                    dest,
                    &program.cursor_ref[*cursor_id]
                        .1
                        .as_ref()
                        .and_then(|x| Some(x.get_name()))
                        .unwrap_or(format!("cursor {}", cursor_id).as_str())
                ),
            ),
            Insn::DecrJumpZero { reg, target_pc } => (
                "DecrJumpZero",
                *reg as i32,
                *target_pc as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("if (--r[{}]==0) goto {}", reg, target_pc),
            ),
            Insn::AggStep {
                func,
                acc_reg,
                delimiter: _,
                col,
            } => (
                "AggStep",
                0,
                *col as i32,
                *acc_reg as i32,
                OwnedValue::Text(Rc::new(func.to_string().into())),
                0,
                format!("accum=r[{}] step(r[{}])", *acc_reg, *col),
            ),
            Insn::AggFinal { register, func } => (
                "AggFinal",
                0,
                *register as i32,
                0,
                OwnedValue::Text(Rc::new(func.to_string().into())),
                0,
                format!("accum=r[{}]", *register),
            ),
            Insn::SorterOpen {
                cursor_id,
                columns,
                order,
            } => {
                let p4 = String::new();
                let to_print: Vec<String> = order
                    .values
                    .iter()
                    .map(|v| match v {
                        OwnedValue::Integer(i) => {
                            if *i == 0 {
                                "B".to_string()
                            } else {
                                "-B".to_string()
                            }
                        }
                        _ => unreachable!(),
                    })
                    .collect();
                (
                    "SorterOpen",
                    *cursor_id as i32,
                    *columns as i32,
                    0,
                    OwnedValue::Text(Rc::new(format!("k({},{})", columns, to_print.join(",")))),
                    0,
                    format!("cursor={}", cursor_id),
                )
            }
            Insn::SorterData {
                cursor_id,
                dest_reg,
                pseudo_cursor,
            } => (
                "SorterData",
                *cursor_id as i32,
                *dest_reg as i32,
                *pseudo_cursor as i32,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                format!("r[{}]=data", dest_reg),
            ),
            Insn::SorterInsert {
                cursor_id,
                record_reg,
            } => (
                "SorterInsert",
                *cursor_id as i32,
                *record_reg as i32,
                0,
                OwnedValue::Integer(0),
                0,
                format!("key=r[{}]", record_reg),
            ),
            Insn::SorterSort {
                cursor_id,
                pc_if_empty,
            } => (
                "SorterSort",
                *cursor_id as i32,
                *pc_if_empty as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                "".to_string(),
            ),
            Insn::SorterNext {
                cursor_id,
                pc_if_next,
            } => (
                "SorterNext",
                *cursor_id as i32,
                *pc_if_next as i32,
                0,
                OwnedValue::Text(Rc::new("".to_string())),
                0,
                "".to_string(),
            ),
            Insn::Function {
                start_reg,
                dest,
                func,
            } => (
                "Function",
                1,
                *start_reg as i32,
                *dest as i32,
                OwnedValue::Text(Rc::new(func.to_string())),
                0,
                format!("r[{}]=func(r[{}..])", dest, start_reg),
            ),
        };
    format!(
        "{:<4}  {:<17}  {:<4}  {:<4}  {:<4}  {:<13}  {:<2}  {}",
        addr,
        &(indent + opcode),
        p1,
        p2,
        p3,
        p4.to_string(),
        p5,
        comment
    )
}

fn get_indent_count(indent_count: usize, curr_insn: &Insn, prev_insn: Option<&Insn>) -> usize {
    let indent_count = if let Some(insn) = prev_insn {
        match insn {
            Insn::RewindAwait { .. } | Insn::SorterSort { .. } => indent_count + 1,
            _ => indent_count,
        }
    } else {
        indent_count
    };

    match curr_insn {
        Insn::NextAsync { .. } | Insn::SorterNext { .. } => indent_count - 1,
        _ => indent_count,
    }
}

fn exec_lower(reg: &OwnedValue) -> Option<OwnedValue> {
    match reg {
        OwnedValue::Text(t) => Some(OwnedValue::Text(Rc::new(t.to_lowercase()))),
        t => Some(t.to_owned()),
    }
}

fn exec_length(reg: &OwnedValue) -> OwnedValue {
    match reg {
        OwnedValue::Text(_) | OwnedValue::Integer(_) | OwnedValue::Float(_) => {
            OwnedValue::Integer(reg.to_string().len() as i64)
        }
        OwnedValue::Blob(blob) => OwnedValue::Integer(blob.len() as i64),
        _ => reg.to_owned(),
    }
}

fn exec_upper(reg: &OwnedValue) -> Option<OwnedValue> {
    match reg {
        OwnedValue::Text(t) => Some(OwnedValue::Text(Rc::new(t.to_uppercase()))),
        t => Some(t.to_owned()),
    }
}

fn exec_abs(reg: &OwnedValue) -> Option<OwnedValue> {
    match reg {
        OwnedValue::Integer(x) => {
            if x < &0 {
                Some(OwnedValue::Integer(-x))
            } else {
                Some(OwnedValue::Integer(*x))
            }
        }
        OwnedValue::Float(x) => {
            if x < &0.0 {
                Some(OwnedValue::Float(-x))
            } else {
                Some(OwnedValue::Float(*x))
            }
        }
        OwnedValue::Null => Some(OwnedValue::Null),
        _ => Some(OwnedValue::Float(0.0)),
    }
}

fn exec_random() -> OwnedValue {
    let mut buf = [0u8; 8];
    getrandom::getrandom(&mut buf).unwrap();
    let random_number = i64::from_ne_bytes(buf);
    OwnedValue::Integer(random_number)
}

// Implements LIKE pattern matching.
fn exec_like(pattern: &str, text: &str) -> bool {
    let re = Regex::new(&pattern.replace('%', ".*").replace('_', ".").to_string()).unwrap();
    re.is_match(text)
}

fn exec_round(reg: &OwnedValue, precision: Option<OwnedValue>) -> OwnedValue {
    let precision = match precision {
        Some(OwnedValue::Text(x)) => x.parse().unwrap_or(0.0),
        Some(OwnedValue::Integer(x)) => x as f64,
        Some(OwnedValue::Float(x)) => x,
        None => 0.0,
        _ => return OwnedValue::Null,
    };

    let reg = match reg {
        OwnedValue::Text(x) => x.parse().unwrap_or(0.0),
        OwnedValue::Integer(x) => *x as f64,
        OwnedValue::Float(x) => *x,
        _ => return reg.to_owned(),
    };

    let precision = if precision < 1.0 { 0.0 } else { precision };
    let multiplier = 10f64.powi(precision as i32);
    OwnedValue::Float(((reg * multiplier).round()) / multiplier)
}

// Implements TRIM pattern matching.
fn exec_trim(reg: &OwnedValue, pattern: Option<OwnedValue>) -> OwnedValue {
    match (reg, pattern) {
        (reg, Some(pattern)) => match reg {
            OwnedValue::Text(_) | OwnedValue::Integer(_) | OwnedValue::Float(_) => {
                let pattern_chars: Vec<char> = pattern.to_string().chars().collect();
                OwnedValue::Text(Rc::new(
                    reg.to_string().trim_matches(&pattern_chars[..]).to_string(),
                ))
            }
            _ => reg.to_owned(),
        },
        (OwnedValue::Text(t), None) => OwnedValue::Text(Rc::new(t.trim().to_string())),
        (reg, _) => reg.to_owned(),
    }
}

// exec_if returns whether you should jump
fn exec_if(reg: &OwnedValue, null_reg: &OwnedValue, not: bool) -> bool {
    match reg {
        OwnedValue::Integer(0) | OwnedValue::Float(0.0) => not,
        OwnedValue::Integer(_) | OwnedValue::Float(_) => !not,
        OwnedValue::Null => match null_reg {
            OwnedValue::Integer(0) | OwnedValue::Float(0.0) => false,
            OwnedValue::Integer(_) | OwnedValue::Float(_) => true,
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        exec_abs, exec_if, exec_length, exec_like, exec_lower, exec_random, exec_round, exec_trim,
        exec_upper, OwnedValue,
    };
    use std::rc::Rc;

    #[test]
    fn test_length() {
        let input_str = OwnedValue::Text(Rc::new(String::from("bob")));
        let expected_len = OwnedValue::Integer(3);
        assert_eq!(exec_length(&input_str), expected_len);

        let input_integer = OwnedValue::Integer(123);
        let expected_len = OwnedValue::Integer(3);
        assert_eq!(exec_length(&input_integer), expected_len);

        let input_float = OwnedValue::Float(123.456);
        let expected_len = OwnedValue::Integer(7);
        assert_eq!(exec_length(&input_float), expected_len);

        // Expected byte array for "example"
        let expected_blob = OwnedValue::Blob(Rc::new(vec![101, 120, 97, 109, 112, 108, 101]));
        let expected_len = OwnedValue::Integer(7);
        assert_eq!(exec_length(&expected_blob), expected_len);
    }

    #[test]
    fn test_trim() {
        let input_str = OwnedValue::Text(Rc::new(String::from("     Bob and Alice     ")));
        let expected_str = OwnedValue::Text(Rc::new(String::from("Bob and Alice")));
        assert_eq!(exec_trim(&input_str, None), expected_str);

        let input_str = OwnedValue::Text(Rc::new(String::from("     Bob and Alice     ")));
        let pattern_str = OwnedValue::Text(Rc::new(String::from("Bob and")));
        let expected_str = OwnedValue::Text(Rc::new(String::from("Alice")));
        assert_eq!(exec_trim(&input_str, Some(pattern_str)), expected_str);
    }

    #[test]
    fn test_upper_case() {
        let input_str = OwnedValue::Text(Rc::new(String::from("Limbo")));
        let expected_str = OwnedValue::Text(Rc::new(String::from("LIMBO")));
        assert_eq!(exec_upper(&input_str).unwrap(), expected_str);

        let input_int = OwnedValue::Integer(10);
        assert_eq!(exec_upper(&input_int).unwrap(), input_int);
        assert_eq!(exec_upper(&OwnedValue::Null).unwrap(), OwnedValue::Null)
    }

    #[test]
    fn test_lower_case() {
        let input_str = OwnedValue::Text(Rc::new(String::from("Limbo")));
        let expected_str = OwnedValue::Text(Rc::new(String::from("limbo")));
        assert_eq!(exec_lower(&input_str).unwrap(), expected_str);

        let input_int = OwnedValue::Integer(10);
        assert_eq!(exec_lower(&input_int).unwrap(), input_int);
        assert_eq!(exec_lower(&OwnedValue::Null).unwrap(), OwnedValue::Null)
    }

    #[test]
    fn test_abs() {
        let int_positive_reg = OwnedValue::Integer(10);
        let int_negative_reg = OwnedValue::Integer(-10);
        assert_eq!(exec_abs(&int_positive_reg).unwrap(), int_positive_reg);
        assert_eq!(exec_abs(&int_negative_reg).unwrap(), int_positive_reg);

        let float_positive_reg = OwnedValue::Integer(10);
        let float_negative_reg = OwnedValue::Integer(-10);
        assert_eq!(exec_abs(&float_positive_reg).unwrap(), float_positive_reg);
        assert_eq!(exec_abs(&float_negative_reg).unwrap(), float_positive_reg);

        assert_eq!(
            exec_abs(&OwnedValue::Text(Rc::new(String::from("a")))).unwrap(),
            OwnedValue::Float(0.0)
        );
        assert_eq!(exec_abs(&OwnedValue::Null).unwrap(), OwnedValue::Null);
    }
    #[test]
    fn test_like() {
        assert!(exec_like("a%", "aaaa"));
        assert!(exec_like("%a%a", "aaaa"));
        assert!(exec_like("%a.a", "aaaa"));
        assert!(exec_like("a.a%", "aaaa"));
        assert!(!exec_like("%a.ab", "aaaa"));
    }

    #[test]
    fn test_random() {
        match exec_random() {
            OwnedValue::Integer(value) => {
                // Check that the value is within the range of i64
                assert!(
                    value >= i64::MIN && value <= i64::MAX,
                    "Random number out of range"
                );
            }
            _ => panic!("exec_random did not return an Integer variant"),
        }
    }

    #[test]
    fn test_exec_round() {
        let input_val = OwnedValue::Float(123.456);
        let expected_val = OwnedValue::Float(123.0);
        assert_eq!(exec_round(&input_val, None), expected_val);

        let input_val = OwnedValue::Float(123.456);
        let precision_val = OwnedValue::Integer(2);
        let expected_val = OwnedValue::Float(123.46);
        assert_eq!(exec_round(&input_val, Some(precision_val)), expected_val);

        let input_val = OwnedValue::Float(123.456);
        let precision_val = OwnedValue::Text(Rc::new(String::from("1")));
        let expected_val = OwnedValue::Float(123.5);
        assert_eq!(exec_round(&input_val, Some(precision_val)), expected_val);

        let input_val = OwnedValue::Text(Rc::new(String::from("123.456")));
        let precision_val = OwnedValue::Integer(2);
        let expected_val = OwnedValue::Float(123.46);
        assert_eq!(exec_round(&input_val, Some(precision_val)), expected_val);

        let input_val = OwnedValue::Integer(123);
        let precision_val = OwnedValue::Integer(1);
        let expected_val = OwnedValue::Float(123.0);
        assert_eq!(exec_round(&input_val, Some(precision_val)), expected_val);
    }

    #[test]
    fn test_exec_if() {
        let reg = OwnedValue::Integer(0);
        let null_reg = OwnedValue::Integer(0);
        assert!(!exec_if(&reg, &null_reg, false));
        assert!(exec_if(&reg, &null_reg, true));

        let reg = OwnedValue::Integer(1);
        let null_reg = OwnedValue::Integer(0);
        assert!(exec_if(&reg, &null_reg, false));
        assert!(!exec_if(&reg, &null_reg, true));

        let reg = OwnedValue::Null;
        let null_reg = OwnedValue::Integer(0);
        assert!(!exec_if(&reg, &null_reg, false));
        assert!(!exec_if(&reg, &null_reg, true));

        let reg = OwnedValue::Null;
        let null_reg = OwnedValue::Integer(1);
        assert!(exec_if(&reg, &null_reg, false));
        assert!(exec_if(&reg, &null_reg, true));

        let reg = OwnedValue::Null;
        let null_reg = OwnedValue::Null;
        assert!(!exec_if(&reg, &null_reg, false));
        assert!(!exec_if(&reg, &null_reg, true));
    }
}
