//! The Hop interpreter: a stack VM over `ir::Program` whose executions are
//! plain Rust values — which is what makes flows suspendable at hops. `At`
//! returns `Outcome::Suspend` with the frames intact; the runtime parks
//! them and resumes with the reply. No coroutines, no host language.

use std::collections::HashMap;

use crate::ir::{BinOp, Function, Instr, Program, UnOp};
use crate::value::{coerce_str, NativeId, Value};

const STEP_LIMIT: u64 = 50_000_000;

#[derive(Clone, Debug)]
pub struct Frame {
    pub fn_idx: usize,
    pub pc: usize,
    pub locals: Vec<Value>,
    pub stack: Vec<Value>,
}

/// One (possibly suspended) execution: a stack of frames.
#[derive(Clone, Debug, Default)]
pub struct Exec {
    pub frames: Vec<Frame>,
}

pub enum Outcome {
    Done(Value),
    /// Hit an `At`: ship `vars` to `target`, park this Exec, resume with
    /// the reply.
    Suspend { target: Value, hop: String, vars: Value },
    Error(String),
}

/// What a host native produces: a value, or a suspension. A suspending
/// native parks the exec exactly like `At` — the reply value resumes the
/// call site as the native's return value. This is how long-running
/// effects (bash, llm) run without blocking the VM.
pub enum NativeOut {
    Val(Value),
    Suspend { target: Value, hop: String, vars: Value },
}

/// Per-VM shared bindings: named fns, natives, module tables, server lets.
/// Shared by every execution on the VM; the runtime seeds them.
#[derive(Default, Clone)]
pub struct Globals {
    pub map: HashMap<String, Value>,
}

impl Globals {
    pub fn get(&self, name: &str) -> Value {
        self.map.get(name).cloned().unwrap_or(Value::Nil)
    }

    pub fn set(&mut self, name: impl Into<String>, v: Value) {
        self.map.insert(name.into(), v);
    }
}

/// Everything side-effectful or contextual the VM can reach. The
/// interpreter handles pure natives itself and defers the rest here.
pub trait Host {
    fn print(&mut self, line: String);
    fn cast(&mut self, target: Value, hop: &str, vars: Value) -> Result<(), String>;
    /// Schedule a new flow on this VM. Arguments were evaluated eagerly at
    /// the spawn site.
    fn spawn(&mut self, callee: Value, args: Vec<Value>) -> Result<(), String>;
    fn session(&mut self) -> Result<Value, String>;
    /// The durable identity behind the session (cookie-minted by hopd).
    /// Defaults to refusal — reducers and store hosts have no user.
    fn user(&mut self) -> Result<Value, String> {
        Err("user() is not available here".into())
    }
    fn native(&mut self, id: NativeId, args: Vec<Value>) -> Result<NativeOut, String>;
    /// A module effect (modules.rs): suspend the flow toward the
    /// platform's executor. The default refusal is the policy for hosts
    /// without a platform — reducers reject every effect by construction.
    fn effect(&mut self, name: &str, hop: &str, vars: Value) -> Result<NativeOut, String> {
        let _ = (hop, vars);
        Err(format!("{name} is not available here"))
    }
}

fn new_frame(f: &Function, fn_idx: usize, caps: &[Value], args: Vec<Value>) -> Frame {
    let mut locals = vec![Value::Nil; f.n_locals as usize];
    for (i, c) in caps.iter().enumerate() {
        locals[i] = c.clone();
    }
    let base = f.n_caps as usize;
    // arity is tolerant: missing args are nil, extras are dropped
    for (i, a) in args.into_iter().enumerate().take(f.n_params as usize) {
        locals[base + i] = a;
    }
    Frame { fn_idx, pc: 0, locals, stack: Vec::new() }
}

impl Exec {
    /// Seed an execution of a plain function (a named fn or segment).
    pub fn call(prog: &Program, fn_idx: usize, args: Vec<Value>) -> Exec {
        let f = &prog.fns[fn_idx];
        Exec { frames: vec![new_frame(f, fn_idx, &[], args)] }
    }

    /// Seed an execution of a closure value.
    pub fn call_value(prog: &Program, callee: &Value, args: Vec<Value>) -> Result<Exec, String> {
        match callee {
            Value::Closure(c) => {
                let f = &prog.fns[c.fn_idx];
                Ok(Exec { frames: vec![new_frame(f, c.fn_idx, &c.caps, args)] })
            }
            other => Err(format!("cannot call a {}", other.kind())),
        }
    }
}

/// Resume a suspended execution with a reply value.
pub fn resume(
    prog: &Program,
    exec: &mut Exec,
    globals: &mut Globals,
    host: &mut dyn Host,
    reply: Value,
) -> Outcome {
    if let Some(top) = exec.frames.last_mut() {
        top.stack.push(reply);
    }
    run(prog, exec, globals, host)
}

pub fn run(
    prog: &Program,
    exec: &mut Exec,
    globals: &mut Globals,
    host: &mut dyn Host,
) -> Outcome {
    let mut steps: u64 = 0;
    loop {
        steps += 1;
        if steps > STEP_LIMIT {
            return Outcome::Error("step limit exceeded (infinite loop?)".into());
        }
        let (fn_idx, pc) = match exec.frames.last() {
            Some(fr) => (fr.fn_idx, fr.pc),
            None => return Outcome::Done(Value::Nil),
        };
        let f = &prog.fns[fn_idx];

        if pc >= f.code.len() {
            // fall off the end = return nil
            exec.frames.pop();
            match exec.frames.last_mut() {
                Some(caller) => caller.stack.push(Value::Nil),
                None => return Outcome::Done(Value::Nil),
            }
            continue;
        }
        exec.frames.last_mut().unwrap().pc += 1;
        let instr = f.code[pc].clone();

        // short-lived borrows of the top frame, so Call/Return can mutate
        // the frame list
        macro_rules! top {
            () => {
                exec.frames.last_mut().unwrap()
            };
        }
        macro_rules! err {
            ($($t:tt)*) => {
                return Outcome::Error(format!("{}:{}: {}", f.name, pc, format!($($t)*)))
            };
        }
        macro_rules! pop {
            () => {
                match top!().stack.pop() {
                    Some(v) => v,
                    None => err!("stack underflow"),
                }
            };
        }
        macro_rules! push {
            ($v:expr) => {{
                let v = $v;
                top!().stack.push(v);
            }};
        }

        match instr {
            Instr::Const(k) => push!(prog.consts[k as usize].clone()),
            Instr::Nil => push!(Value::Nil),
            Instr::True => push!(Value::Bool(true)),
            Instr::False => push!(Value::Bool(false)),
            Instr::LoadLocal(i) => push!(top!().locals[i as usize].clone()),
            Instr::StoreLocal(i) => {
                let v = pop!();
                top!().locals[i as usize] = v;
            }
            Instr::LoadGlobal(k) => push!(globals.get(prog.const_str(k))),
            Instr::StoreGlobal(k) => {
                let v = pop!();
                globals.set(prog.const_str(k).to_string(), v);
            }
            Instr::MakeArray(n) => {
                let fr = top!();
                let at = fr.stack.len().saturating_sub(n as usize);
                let items: Vec<Value> = fr.stack.split_off(at);
                fr.stack.push(Value::array(items));
            }
            Instr::MakeMap(n) => {
                let fr = top!();
                let at = fr.stack.len().saturating_sub(n as usize * 2);
                let flat: Vec<Value> = fr.stack.split_off(at);
                let mut m = std::collections::BTreeMap::new();
                for pair in flat.chunks(2) {
                    let (k, v) = (pair[0].clone(), pair[1].clone());
                    if !k.is_scalar() {
                        err!("map key must be scalar, got {}", k.kind());
                    }
                    if !matches!(v, Value::Nil) {
                        m.insert(k, v);
                    }
                }
                top!().stack.push(Value::map(m));
            }
            Instr::GetIndex => {
                let key = pop!();
                let obj = pop!();
                let v = match (&obj, &key) {
                    (Value::Array(a), Value::Int(i)) => {
                        let a = a.borrow();
                        if *i >= 0 && (*i as usize) < a.len() {
                            a[*i as usize].clone()
                        } else {
                            Value::Nil
                        }
                    }
                    (Value::Map(m), k) if k.is_scalar() => {
                        m.borrow().get(k).cloned().unwrap_or(Value::Nil)
                    }
                    (Value::Nil, _) => err!("indexing nil"),
                    (o, k) => err!("cannot index {} with {}", o.kind(), k.kind()),
                };
                push!(v);
            }
            Instr::SetIndex => {
                let v = pop!();
                let key = pop!();
                let obj = pop!();
                match (&obj, &key) {
                    (Value::Array(a), Value::Int(i)) => {
                        let mut a = a.borrow_mut();
                        let i = *i;
                        if i < 0 || (i as usize) > a.len() {
                            err!("array index {} out of range 0..={}", i, a.len());
                        }
                        if (i as usize) == a.len() {
                            a.push(v);
                        } else {
                            a[i as usize] = v;
                        }
                    }
                    (Value::Map(m), k) if k.is_scalar() => {
                        if matches!(v, Value::Nil) {
                            m.borrow_mut().remove(k);
                        } else {
                            m.borrow_mut().insert(k.clone(), v);
                        }
                    }
                    (o, k) => err!("cannot index-assign {} with {}", o.kind(), k.kind()),
                }
            }
            Instr::GetField(k) => {
                let obj = pop!();
                let name = prog.const_str(k);
                match &obj {
                    Value::Map(_) => push!(obj.get_field(name)),
                    Value::Native(NativeId::StoreCall) => {
                        match crate::builtins::store_field(name) {
                            Some(v) => push!(v),
                            None => err!("store has no field .{}", name),
                        }
                    }
                    Value::Nil => err!("field .{} of nil", name),
                    o => err!("field .{} of {}", name, o.kind()),
                }
            }
            Instr::SetField(k) => {
                let v = pop!();
                let obj = pop!();
                let name = prog.const_str(k);
                if let Err(e) = obj.set_field(name, v) {
                    err!("{e}");
                }
            }
            Instr::BinOp(op) => {
                let b = pop!();
                let a = pop!();
                match bin_op(op, a, b) {
                    Ok(v) => push!(v),
                    Err(e) => err!("{e}"),
                }
            }
            Instr::UnOp(op) => {
                let a = pop!();
                let v = match op {
                    UnOp::Not => Value::Bool(!a.truthy()),
                    UnOp::Neg => match a {
                        Value::Int(i) => Value::Int(-i),
                        Value::Float(x) => Value::Float(-x),
                        o => err!("cannot negate {}", o.kind()),
                    },
                };
                push!(v);
            }
            Instr::Jump(d) => {
                let fr = top!();
                fr.pc = (fr.pc as i64 + d as i64) as usize;
            }
            Instr::JumpIfFalse(d) => {
                let v = pop!();
                if !v.truthy() {
                    let fr = top!();
                    fr.pc = (fr.pc as i64 + d as i64) as usize;
                }
            }
            Instr::Dup => {
                let v = match top!().stack.last() {
                    Some(v) => v.clone(),
                    None => err!("stack underflow"),
                };
                push!(v);
            }
            Instr::Pop => {
                pop!();
            }
            Instr::Call(nargs) => {
                let (callee, args) = {
                    let fr = top!();
                    let at = fr.stack.len().saturating_sub(nargs as usize);
                    let args: Vec<Value> = fr.stack.split_off(at);
                    let callee = match fr.stack.pop() {
                        Some(c) => c,
                        None => err!("stack underflow (callee)"),
                    };
                    (callee, args)
                };
                match callee {
                    Value::Closure(c) => {
                        let callee_fn = &prog.fns[c.fn_idx];
                        let new = new_frame(callee_fn, c.fn_idx, &c.caps, args);
                        exec.frames.push(new);
                    }
                    Value::Native(id) => match call_native(id, args, host) {
                        Ok(NativeOut::Val(v)) => push!(v),
                        Ok(NativeOut::Suspend { target, hop, vars }) => {
                            return Outcome::Suspend { target, hop, vars }
                        }
                        Err(e) => err!("{e}"),
                    },
                    Value::Lib(name) => {
                        let out = match crate::modules::registry().get(name) {
                            Some(crate::modules::NativeKind::Pure(f)) => {
                                f(args).map(NativeOut::Val)
                            }
                            Some(crate::modules::NativeKind::Effect { hop, vars }) => {
                                vars(args).and_then(|v| host.effect(name, hop, v))
                            }
                            None => Err(format!("unknown native {name}")),
                        };
                        match out {
                            Ok(NativeOut::Val(v)) => push!(v),
                            Ok(NativeOut::Suspend { target, hop, vars }) => {
                                return Outcome::Suspend { target, hop, vars }
                            }
                            Err(e) => err!("{e}"),
                        }
                    }
                    Value::Nil => err!("calling nil (unknown function?)"),
                    o => err!("cannot call a {}", o.kind()),
                }
            }
            Instr::Closure(fn_i, ncaps) => {
                let fr = top!();
                let at = fr.stack.len().saturating_sub(ncaps as usize);
                let caps: Vec<Value> = fr.stack.split_off(at);
                fr.stack.push(Value::Closure(std::rc::Rc::new(
                    crate::value::ClosureVal { fn_idx: fn_i as usize, caps },
                )));
            }
            Instr::IterNew => {
                let obj = pop!();
                let pairs: Vec<Value> = match &obj {
                    Value::Array(a) => a
                        .borrow()
                        .iter()
                        .enumerate()
                        .map(|(i, v)| Value::array(vec![Value::Int(i as i64), v.clone()]))
                        .collect(),
                    Value::Map(m) => m
                        .borrow()
                        .iter()
                        .map(|(k, v)| Value::array(vec![k.clone(), v.clone()]))
                        .collect(),
                    o => err!("cannot iterate a {}", o.kind()),
                };
                push!(Value::array(pairs));
            }
            Instr::IterNext(d) => {
                // stack: iter idx → (iter idx+1 k v) or jump with both popped
                let idx = pop!();
                let iter = pop!();
                let i = match idx {
                    Value::Int(i) => i,
                    _ => err!("iterator index corrupted"),
                };
                let pair = match &iter {
                    Value::Array(a) => {
                        let a = a.borrow();
                        if (i as usize) < a.len() {
                            Some(a[i as usize].clone())
                        } else {
                            None
                        }
                    }
                    _ => err!("iterator corrupted"),
                };
                match pair {
                    Some(Value::Array(kv)) => {
                        let (k, v) = {
                            let kv = kv.borrow();
                            (kv[0].clone(), kv[1].clone())
                        };
                        let fr = top!();
                        fr.stack.push(iter.clone());
                        fr.stack.push(Value::Int(i + 1));
                        fr.stack.push(k);
                        fr.stack.push(v);
                    }
                    Some(_) => err!("iterator corrupted"),
                    None => {
                        let fr = top!();
                        fr.pc = (fr.pc as i64 + d as i64) as usize;
                    }
                }
            }
            Instr::Return => {
                let ret = pop!();
                exec.frames.pop();
                match exec.frames.last_mut() {
                    Some(caller) => caller.stack.push(ret),
                    None => return Outcome::Done(ret),
                }
            }
            Instr::At(k) => {
                let vars = pop!();
                let target = pop!();
                let hop = prog.const_str(k).to_string();
                return Outcome::Suspend { target, hop, vars };
            }
            Instr::Cast(k) => {
                let vars = pop!();
                let target = pop!();
                let hop = prog.const_str(k).to_string();
                if let Err(e) = host.cast(target, &hop, vars) {
                    err!("{e}");
                }
            }
            Instr::Spawn => {
                // stack: callee args-array
                let args_v = pop!();
                let callee = pop!();
                let args = match &args_v {
                    Value::Array(a) => a.borrow().clone(),
                    _ => err!("spawn arguments corrupted"),
                };
                if let Err(e) = host.spawn(callee, args) {
                    err!("{e}");
                }
            }
            Instr::Session => match host.session() {
                Ok(v) => push!(v),
                Err(e) => err!("{e}"),
            },
            Instr::User => match host.user() {
                Ok(v) => push!(v),
                Err(e) => err!("{e}"),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

fn bin_op(op: BinOp, a: Value, b: Value) -> Result<Value, String> {
    use BinOp::*;
    match op {
        Add | Sub | Mul => arith(op, a, b),
        Div => match (num(&a), num(&b)) {
            (Some(x), Some(y)) => Value::float(x / y),
            _ => Err(format!("cannot divide {} by {}", a.kind(), b.kind())),
        },
        Mod => match (&a, &b) {
            (Value::Int(x), Value::Int(y)) => {
                if *y == 0 {
                    Err("mod by zero".into())
                } else {
                    Ok(Value::Int(x.rem_euclid(*y)))
                }
            }
            _ => match (num(&a), num(&b)) {
                (Some(x), Some(y)) => Value::float(x.rem_euclid(y)),
                _ => Err(format!("cannot mod {} by {}", a.kind(), b.kind())),
            },
        },
        Concat => match (&a, &b) {
            (Value::Str(_) | Value::Int(_) | Value::Float(_), Value::Str(_) | Value::Int(_) | Value::Float(_)) => {
                Ok(Value::str(format!("{}{}", coerce_str(&a), coerce_str(&b))))
            }
            _ => Err(format!("cannot concat {} and {}", a.kind(), b.kind())),
        },
        Eq => Ok(Value::Bool(a == b)),
        Ne => Ok(Value::Bool(a != b)),
        Lt | Le | Gt | Ge => {
            let ord = match (&a, &b) {
                (Value::Int(x), Value::Int(y)) => x.partial_cmp(y),
                (Value::Str(x), Value::Str(y)) => x.partial_cmp(y),
                _ => match (num(&a), num(&b)) {
                    (Some(x), Some(y)) => x.partial_cmp(&y),
                    _ => return Err(format!("cannot compare {} and {}", a.kind(), b.kind())),
                },
            };
            let Some(ord) = ord else {
                return Err("incomparable values".into());
            };
            Ok(Value::Bool(match op {
                Lt => ord.is_lt(),
                Le => ord.is_le(),
                Gt => ord.is_gt(),
                Ge => ord.is_ge(),
                _ => unreachable!(),
            }))
        }
    }
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

fn arith(op: BinOp, a: Value, b: Value) -> Result<Value, String> {
    match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => {
            let r = match op {
                BinOp::Add => x.checked_add(*y),
                BinOp::Sub => x.checked_sub(*y),
                BinOp::Mul => x.checked_mul(*y),
                _ => unreachable!(),
            };
            r.map(Value::Int).ok_or_else(|| "integer overflow".into())
        }
        _ => match (num(&a), num(&b)) {
            (Some(x), Some(y)) => {
                let r = match op {
                    BinOp::Add => x + y,
                    BinOp::Sub => x - y,
                    BinOp::Mul => x * y,
                    _ => unreachable!(),
                };
                Value::float(r)
            }
            _ => Err(format!(
                "cannot {:?} {} and {}",
                op,
                a.kind(),
                b.kind()
            )),
        },
    }
}

// ---------------------------------------------------------------------------
// Pure natives (contextual ones go to the Host)
// ---------------------------------------------------------------------------

fn call_native(id: NativeId, mut args: Vec<Value>, host: &mut dyn Host) -> Result<NativeOut, String> {
    let r: Result<Value, String> = match id {
        NativeId::Print => {
            let line = args.iter().map(coerce_str).collect::<Vec<_>>().join(" ");
            host.print(line);
            Ok(Value::Nil)
        }
        NativeId::Error => {
            let v = args.first().cloned().unwrap_or(Value::Nil);
            Err(coerce_str(&v))
        }
        NativeId::Tostring => {
            let v = args.first().cloned().unwrap_or(Value::Nil);
            Ok(Value::str(coerce_str(&v)))
        }
        NativeId::Tonumber => {
            let v = args.first().cloned().unwrap_or(Value::Nil);
            Ok(match &v {
                Value::Int(_) | Value::Float(_) => v,
                Value::Str(s) => {
                    let s = s.trim();
                    if let Ok(i) = s.parse::<i64>() {
                        Value::Int(i)
                    } else if let Ok(f) = s.parse::<f64>() {
                        if f.is_nan() {
                            Value::Nil
                        } else {
                            Value::Float(f)
                        }
                    } else {
                        Value::Nil
                    }
                }
                _ => Value::Nil,
            })
        }
        NativeId::Push => {
            if args.len() < 2 {
                return Err("push(array, value)".into());
            }
            let v = args.pop().unwrap();
            match &args[0] {
                Value::Array(a) => {
                    a.borrow_mut().push(v);
                    Ok(Value::Nil)
                }
                o => Err(format!("push into {}", o.kind())),
            }
        }
        NativeId::Len => match args.first() {
            Some(Value::Array(a)) => Ok(Value::Int(a.borrow().len() as i64)),
            Some(Value::Map(m)) => Ok(Value::Int(m.borrow().len() as i64)),
            Some(Value::Str(s)) => Ok(Value::Int(s.len() as i64)),
            Some(Value::Nil) | None => Ok(Value::Int(0)),
            Some(o) => Err(format!("len of {}", o.kind())),
        },
        NativeId::SortBy => {
            // sort_by(array)             natural order
            // sort_by(array, [fields])   by field chain, stable
            let keys: Vec<String> = match args.get(1) {
                Some(Value::Array(ks)) => ks
                    .borrow()
                    .iter()
                    .map(|k| k.as_str().map(str::to_string).ok_or("sort_by keys must be strings"))
                    .collect::<Result<_, _>>()?,
                _ => Vec::new(),
            };
            match args.first() {
                Some(Value::Array(a)) => {
                    a.borrow_mut().sort_by(|x, y| {
                        if keys.is_empty() {
                            x.cmp(y)
                        } else {
                            keys.iter()
                                .map(|k| x.get_field(k).cmp(&y.get_field(k)))
                                .find(|o| !o.is_eq())
                                .unwrap_or(std::cmp::Ordering::Equal)
                        }
                    });
                    Ok(Value::Nil)
                }
                _ => Err("sort_by expects an array".into()),
            }
        }
        NativeId::Floor => match args.first() {
            Some(Value::Int(i)) => Ok(Value::Int(*i)),
            Some(Value::Float(f)) => Ok(Value::Int(f.floor() as i64)),
            _ => Err("floor expects a number".into()),
        },
        // type(v) → the kind name ("nil", "int", "map", …) — the guard
        // for data of unknown shape (e.g. whatever json.decode returned).
        NativeId::TypeOf => Ok(Value::str(
            args.first().map(Value::kind).unwrap_or("nil"),
        )),
        // schema shape constructors are pure data builders
        NativeId::ShapeMap | NativeId::ShapeList | NativeId::ShapeDeque => {
            let of = args.first().cloned().unwrap_or(Value::Nil);
            let k = match id {
                NativeId::ShapeMap => "map",
                NativeId::ShapeList => "list",
                _ => "deque",
            };
            let mut m = std::collections::BTreeMap::new();
            m.insert(Value::str("k"), Value::str(k));
            if !matches!(of, Value::Nil) {
                m.insert(Value::str("of"), of);
            }
            Ok(Value::map(m))
        }
        NativeId::ShapeRecord => {
            let fields = args.first().cloned().unwrap_or(Value::Nil);
            let mut m = std::collections::BTreeMap::new();
            m.insert(Value::str("k"), Value::str("record"));
            m.insert(Value::str("fields"), fields);
            Ok(Value::map(m))
        }
        // parametrized collecting navigators: pure data.
        //   store.where(field)             the field exists
        //   store.where(field, v)          equality
        //   store.where(field, op, v)      op ∈ == != < <= > >=
        //   store.slice(start, end)        nil bounds are open
        NativeId::NavWhere => {
            let field = args.first().cloned().unwrap_or(Value::Nil);
            let mut parts = vec![Value::str("where"), field];
            match (args.get(1), args.get(2)) {
                (None, _) => {}
                (Some(v), None) => {
                    parts.push(Value::str("=="));
                    parts.push(v.clone());
                }
                (Some(op), Some(v)) => {
                    parts.push(op.clone());
                    parts.push(v.clone());
                }
            }
            Ok(Value::tagged("nav", Value::array(parts)))
        }
        NativeId::NavSlice => Ok(Value::tagged(
            "nav",
            Value::array(vec![
                Value::str("slice"),
                args.first().cloned().unwrap_or(Value::Nil),
                args.get(1).cloned().unwrap_or(Value::Nil),
            ]),
        )),
        // terminal navigator constructors: pure data, meaning applied by
        // the store when it sees them at the end of a path
        NativeId::NavSet | NativeId::NavAdd | NativeId::NavPush => {
            let op = match id {
                NativeId::NavSet => "set",
                NativeId::NavAdd => "add",
                _ => "push",
            };
            let v = args.first().cloned().unwrap_or(Value::Nil);
            Ok(Value::tagged("term", Value::array(vec![Value::str(op), v])))
        }
        other => return host.native(other, args),
    };
    r.map(NativeOut::Val)
}
