//! Mission 5 of the campaign: the village fires.
//!
//! The game's attack check says yes to a fire for any creature, so creatures without a water
//! attack walk into the fires to fight them and burn. `ICMod.dll`'s check is wrapped so that
//! a fire is only a target for a creature with water artillery: a ranged attack whose
//! `special` attribute is 2 (the Archerfish's, and only its).
//!
//! (Creatures with water artillery can still end up fighting a fire in melee when they get
//! closer than the attack's minimum range; no fix for that is known.)

use crate::cpu::{EAX, Emu, Res};
use crate::hooks::Hook;
use crate::win32::{Args, Handler};

pub(super) const HOOKS: &[Hook] = &[
    // CanAttack(attacker in eax, target), checked before it runs (`can_attack`).
    Hook {
        module: "icmod",
        rva: CAN_ATTACK,
        expect: &[
            0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x14, 0x56, 0x8B, 0xF0, 0x56, 0xE8,
        ],
        name: "ICMod_can_attack",
        points_to: None,
        on_patch: Some(patched),
    },
];

#[derive(Default)]
pub struct State {
    /// Where ICMod.dll is loaded, and the trampoline that runs the original CanAttack.
    base: u32,
    can_attack_original: u32,
}

pub(super) fn lookup(name: &str) -> Option<(usize, Handler)> {
    Some(match name {
        "ICMod_can_attack" => (1, can_attack),
        _ => return None,
    })
}

// CanAttack (stdcall, attacker entity in eax, target entity on the stack, answer in al) is
// what a creature's target search asks of every nearby entity. A fire is an entity whose
// ICMod controller is a FireController. The attribute loader keeps each ranged attack of a
// creature as a 96-byte record in its combat extension's list at +12..+16, `special` at
// offset 36.

const CAN_ATTACK: u32 = 0x72c17;
/// The original's first instructions (push ebp; mov ebp, esp; sub esp, 20), which the patch
/// overwrites, run from the trampoline before it jumps back.
const CAN_ATTACK_PROLOGUE: [u8; 6] = [0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x14];
/// FireController's vtable, and the function returning a controller's combat extension.
const FIRE_CONTROLLER_VTABLE: u32 = 0xdf13c;
const COMBAT_EXT: u32 = 0x16e2c;
/// An entity's ICMod controller.
const ENTITY_CONTROLLER: u32 = 176;
const RANGED_ATTACKS: u32 = 12;
const ATTACK_RECORD: u32 = 96;
const ATTACK_SPECIAL: u32 = 36;
const SPECIAL_WATER: u32 = 2;

fn patched(e: &mut Emu, base: u32) {
    let original = trampoline(e, base, CAN_ATTACK, &CAN_ATTACK_PROLOGUE);
    let st = &mut e.win.games.impossible_creatures.fire;
    (st.base, st.can_attack_original) = (base, original);
}

/// Code that runs a patched function's overwritten first instructions (`prologue`) and jumps
/// back to the rest of it: calling it runs the original. It gets a page of its own, as code
/// on a page also written to as data would throw away the instruction caches on every write.
fn trampoline(e: &mut Emu, base: u32, rva: u32, prologue: &[u8]) -> u32 {
    let at = e.reserve(0x1000);
    e.mem.map(at, 0x1000);
    let n = prologue.len() as u32;
    let back = (base + rva + n).wrapping_sub(at + n + 5);
    let mut code = prologue.to_vec();
    code.push(0xE9); // jmp rel32 back into the original
    code.extend(back.to_le_bytes());
    let _ = e.mem.write_bytes(at, &code);
    at
}

fn can_attack(e: &mut Emu, a: &Args) -> Res<u32> {
    let (attacker, target) = (e.regs[EAX], a[0]);
    let st = &e.win.games.impossible_creatures.fire;
    let (base, original) = (st.base, st.can_attack_original);
    if is_fire(e, base, target) && !has_water_attack(e, base, attacker)? {
        return Ok(e.regs[EAX] & !0xFF); // al = false
    }
    e.regs[EAX] = attacker;
    e.call_function(original, &[target])
}

/// Is the entity one of the fires (its ICMod controller a FireController)?
fn is_fire(e: &Emu, base: u32, entity: u32) -> bool {
    let rd = |p: u32| e.mem.read(p, 4);
    entity != 0
        && rd(entity + ENTITY_CONTROLLER)
            .and_then(|c| if c == 0 { Ok(0) } else { rd(c) })
            .is_ok_and(|vt| vt == base + FIRE_CONTROLLER_VTABLE)
}

/// Does the creature have a ranged attack that spits water?
fn has_water_attack(e: &mut Emu, base: u32, entity: u32) -> Res<bool> {
    if entity == 0 {
        return Ok(false);
    }
    let controller = e.mem.read(entity + ENTITY_CONTROLLER, 4)?;
    let combat = e.call_function(base + COMBAT_EXT, &[controller])?;
    if combat == 0 {
        return Ok(false);
    }
    let (first, end) = (
        e.mem.read(combat + RANGED_ATTACKS, 4)?,
        e.mem.read(combat + RANGED_ATTACKS + 4, 4)?,
    );
    let mut at = first;
    while at < end && end - at < 64 * ATTACK_RECORD {
        if e.mem.read(at + ATTACK_SPECIAL, 4)? == SPECIAL_WATER {
            return Ok(true);
        }
        at += ATTACK_RECORD;
    }
    Ok(false)
}
