// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `DropView` AST -> `Program` compilation (#552). A single
//! `Opcode::DropView` instruction removes the view's `sqlite_master` row
//! and bumps the schema cookie; a view has no pages to free.

use crate::codegen::row::select::CodegenError;
use crate::codegen::row::Emitter;
use crate::parser::ast::DropView;
use crate::vm::row::{Instruction, Opcode, Program, P4};

/// Compiles `DROP VIEW` into a single `Opcode::DropView` instruction.
pub fn compile_drop_view(dv: &DropView) -> Result<Program, CodegenError> {
    let mut em = Emitter::new();
    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);
    em.emit(Instruction::with_p4(
        Opcode::DropView,
        0,
        0,
        0,
        P4::DropView {
            name: dv.name.clone(),
        },
    ));
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    Ok(em.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::row::error::{parse_drop_view, ParseOutcome};

    #[test]
    fn compiles_to_init_drop_view_halt() {
        let dv = match parse_drop_view("DROP VIEW v") {
            ParseOutcome::Accepted(d) => d,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let program = compile_drop_view(&dv).unwrap();
        let opcodes: Vec<Opcode> = program.instructions.iter().map(|i| i.opcode).collect();
        assert_eq!(opcodes, vec![Opcode::Init, Opcode::DropView, Opcode::Halt]);
        assert!(matches!(&program.instructions[1].p4, P4::DropView { name } if name == "v"));
    }
}
