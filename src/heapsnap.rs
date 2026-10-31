//! V8 heap snapshots (`.heapsnapshot` JSON, plain or gzip): classes, retained sizes from the
//! dominator tree, detached DOM, shortest retainer paths, diffs and leak suspects (SPEC §8.6).
//! The big integer arrays stream straight into columns: no per-node JSON values.

use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const NONE: u32 = u32::MAX;
const ATTACHED: u8 = 1;
const DETACHED: u8 = 2;

/// A parsed snapshot with its dominator tree. Nodes are ordinals (array index / node field count).
#[derive(Default)]
pub struct Snapshot {
    /// Every string back to back (millions of small allocations otherwise), and where each ends.
    strings: String,
    string_ends: Vec<usize>,
    node_types: Vec<String>,
    edge_types: Vec<String>,
    ntype: Vec<u8>,
    name: Vec<u32>,
    id: Vec<u32>,
    size: Vec<u32>,
    first_edge: Vec<u32>,
    trace: Vec<u32>,
    state: Vec<u8>,
    has_detachedness: bool,
    etype: Vec<u8>,
    ename: Vec<u32>,
    eto: Vec<u32>,
    /// [node, script id, line, column] (0-based), sorted by node.
    locations: Vec<[u32; 4]>,
    /// Script id → URL, read from the Script objects the snapshot holds.
    scripts: HashMap<u32, String>,
    /// [function id, name, script name, script id, line, column] (1-based, 0 unknown).
    trace_infos: Vec<[u32; 6]>,
    /// Allocation trace node id → (function info index, parent trace node id).
    trace_nodes: HashMap<u32, (u32, u32)>,
    root: u32,
    /// Edge into each node on its shortest path from the root; NONE for the root and unreachable nodes.
    parent: Vec<u32>,
    retained: Vec<u64>,
    dom_first: Vec<u32>,
    dom_kids: Vec<u32>,
    class: Vec<u32>,
    classes: Vec<Class>,
}

struct Class {
    name: String,
    ntype: u8,
    detached: bool,
    /// [script id, line, column], 0-based.
    loc: Option<[u32; 3]>,
    count: u64,
    self_size: u64,
    retained: u64,
    largest: u32,
}

/// Parses a snapshot and builds its dominator tree.
pub fn read(path: &Path) -> Result<Snapshot> {
    let mut s = parse(path)?;
    s.analyse();
    Ok(s)
}

/// Only the node ids, sorted: all `leaks` needs from S0 and S1.
pub fn ids(path: &Path) -> Result<Vec<u32>> {
    Ok(parse(path)?.sorted_ids())
}

impl Snapshot {
    fn sorted_ids(&self) -> Vec<u32> {
        let mut ids = self.id.clone();
        ids.sort_unstable();
        ids
    }

    fn str(&self, i: u32) -> &str {
        let i = i as usize;
        match self.string_ends.get(i) {
            Some(&end) => &self.strings[if i == 0 { 0 } else { self.string_ends[i - 1] }..end],
            None => "",
        }
    }

    fn node_type(&self, name: &str) -> u8 {
        self.node_types.iter().position(|t| t == name).map(|i| i as u8).unwrap_or(u8::MAX)
    }

    fn edge_type(&self, name: &str) -> u8 {
        self.edge_types.iter().position(|t| t == name).map(|i| i as u8).unwrap_or(u8::MAX)
    }

    fn type_name(&self, v: usize) -> &str {
        self.node_types.get(self.ntype[v] as usize).map(String::as_str).unwrap_or("?")
    }

    fn edges(&self, v: usize) -> std::ops::Range<usize> {
        self.first_edge[v] as usize..self.first_edge[v + 1] as usize
    }

    /// The node an edge starts from.
    fn owner(&self, e: usize) -> usize {
        self.first_edge.partition_point(|&f| f as usize <= e) - 1
    }

    /// The target of `v`'s first named edge called `name`.
    fn child(&self, v: usize, name: &str) -> Option<usize> {
        let (element, hidden) = (self.edge_type("element"), self.edge_type("hidden"));
        self.edges(v).find(|&e| self.etype[e] != element && self.etype[e] != hidden && self.str(self.ename[e]) == name).map(|e| self.eto[e] as usize)
    }

    fn analyse(&mut self) {
        let n = self.ntype.len();
        let root = self.root as usize;
        let (weak, shortcut) = (self.edge_type("weak"), self.edge_type("shortcut"));

        // Shortest paths over strong edges, user roots (the root's shortcuts to each page's native
        // context or global) first, as DevTools does, so page objects show their path from the page.
        let mut parent = vec![NONE; n];
        let mut seen = vec![false; n];
        let mut queue: Vec<u32> = Vec::with_capacity(n);
        seen[root] = true;
        for pass in 0..2 {
            let mut head = queue.len();
            for e in self.edges(root) {
                let to = self.eto[e] as usize;
                if seen[to] || self.etype[e] == weak || (pass == 0) != (self.etype[e] == shortcut) {
                    continue;
                }
                seen[to] = true;
                parent[to] = e as u32;
                queue.push(to as u32);
            }
            while head < queue.len() {
                let v = queue[head] as usize;
                head += 1;
                for e in self.edges(v) {
                    let to = self.eto[e] as usize;
                    if seen[to] || self.etype[e] == weak {
                        continue;
                    }
                    seen[to] = true;
                    parent[to] = e as u32;
                    queue.push(to as u32);
                }
            }
        }
        drop((seen, queue));

        // Dominators (Cooper, Harvey, Kennedy) over essential edges: never weak, shortcuts only from the root.
        let essential = |v: usize, e: usize| self.etype[e] != weak && (self.etype[e] != shortcut || v == root);
        let mut po_of = vec![NONE; n];
        let mut order: Vec<u32> = Vec::with_capacity(n);
        let mut stack: Vec<(u32, u32)> = vec![(root as u32, self.first_edge[root])];
        let mut visited = vec![false; n];
        visited[root] = true;
        while let Some(&(v, e)) = stack.last() {
            let v = v as usize;
            if e < self.first_edge[v + 1] {
                stack.last_mut().unwrap().1 += 1;
                let to = self.eto[e as usize] as usize;
                if !visited[to] && essential(v, e as usize) {
                    visited[to] = true;
                    stack.push((to as u32, self.first_edge[to]));
                }
            } else {
                po_of[v] = order.len() as u32;
                order.push(v as u32);
                stack.pop();
            }
        }
        drop((stack, visited));
        // Retainers of each node as post-order indices, from reachable nodes only.
        let mut rfirst = vec![0u32; n + 1];
        for &v in &order {
            for e in self.edges(v as usize) {
                if essential(v as usize, e) {
                    rfirst[self.eto[e] as usize + 1] += 1;
                }
            }
        }
        for i in 0..n {
            rfirst[i + 1] += rfirst[i];
        }
        let mut fill = rfirst.clone();
        let mut rpo = vec![0u32; rfirst[n] as usize];
        for (po, &v) in order.iter().enumerate() {
            for e in self.edges(v as usize) {
                if essential(v as usize, e) {
                    let to = self.eto[e] as usize;
                    rpo[fill[to] as usize] = po as u32;
                    fill[to] += 1;
                }
            }
        }
        drop(fill);
        let top = order.len() - 1;
        let mut dom = vec![NONE; order.len()];
        dom[top] = top as u32;
        let mut changed = true;
        while changed {
            changed = false;
            for po in (0..top).rev() {
                // Dominators only move up the tree: one that reached the root is final.
                if dom[po] == top as u32 {
                    continue;
                }
                let v = order[po] as usize;
                let mut d = NONE;
                for &r in &rpo[rfirst[v] as usize..rfirst[v + 1] as usize] {
                    if dom[r as usize] != NONE {
                        d = if d == NONE { r } else { intersect(&dom, r, d) };
                    }
                }
                if dom[po] != d {
                    dom[po] = d;
                    changed = true;
                }
            }
        }
        drop((rpo, rfirst));
        // Nodes only weak edges or nothing reach hang off the root, as in DevTools.
        let mut idom = vec![root as u32; n];
        let mut retained: Vec<u64> = self.size.iter().map(|&s| s as u64).collect();
        for po in 0..top {
            let (v, d) = (order[po] as usize, order[dom[po] as usize] as usize);
            idom[v] = d as u32;
            retained[d] += retained[v];
        }
        for v in 0..n {
            if po_of[v] == NONE && v != root {
                retained[root] += retained[v];
            }
        }
        drop((order, dom, po_of));
        let mut dom_first = vec![0u32; n + 1];
        for v in (0..n).filter(|&v| v != root) {
            dom_first[idom[v] as usize + 1] += 1;
        }
        for i in 0..n {
            dom_first[i + 1] += dom_first[i];
        }
        let mut fill = dom_first.clone();
        let mut dom_kids = vec![0u32; n.saturating_sub(1)];
        for v in (0..n).filter(|&v| v != root) {
            let d = idom[v] as usize;
            dom_kids[fill[d] as usize] = v as u32;
            fill[d] += 1;
        }
        drop((fill, idom));
        self.parent = parent;
        self.retained = retained;
        self.dom_first = dom_first;
        self.dom_kids = dom_kids;
        self.resolve_scripts();
        self.propagate_dom_state();
        self.build_classes();
    }

    /// Script URLs: a closure's `shared` → SharedFunctionInfo → `script` → Script, named by its URL.
    fn resolve_scripts(&mut self) {
        let closure = self.node_type("closure");
        let mut scripts = HashMap::new();
        for &[v, sid, _, _] in &self.locations {
            let v = v as usize;
            if scripts.contains_key(&sid) || self.ntype[v] != closure {
                continue;
            }
            let Some(sfi) = self.child(v, "shared") else { continue };
            let Some(script) = self.child(sfi, "script").or_else(|| self.child(sfi, "script_or_debug_info")) else { continue };
            let mut url = self.str(self.name[script]);
            if url.is_empty() || url.starts_with("system / ") {
                url = self.child(script, "name").map(|s| self.str(self.name[s])).unwrap_or("");
            }
            if !url.is_empty() && !url.starts_with("system / ") {
                scripts.insert(sid, url.to_string());
            }
        }
        self.scripts = scripts;
    }

    /// Detachedness the DevTools way: native nodes with a known state pass it to their native
    /// children; attached wins. Without the field, names that start with "Detached " are detached.
    fn propagate_dom_state(&mut self) {
        let n = self.ntype.len();
        let native = self.node_type("native");
        if !self.has_detachedness {
            for v in 0..n {
                if self.ntype[v] == native && self.str(self.name[v]).starts_with("Detached ") {
                    self.state[v] = DETACHED;
                }
            }
            return;
        }
        let mut visited = vec![false; n];
        let (mut attached, mut detached) = (Vec::new(), Vec::new());
        for v in 0..n {
            let st = self.state[v];
            if st == ATTACHED || st == DETACHED {
                self.mark(&mut visited, native, v, st, if st == ATTACHED { &mut attached } else { &mut detached });
            }
        }
        while let Some(v) = attached.pop() {
            for e in self.edges(v) {
                let to = self.eto[e] as usize;
                self.mark(&mut visited, native, to, ATTACHED, &mut attached);
            }
        }
        while let Some(v) = detached.pop() {
            if self.state[v] == ATTACHED {
                continue;
            }
            for e in self.edges(v) {
                let to = self.eto[e] as usize;
                self.mark(&mut visited, native, to, DETACHED, &mut detached);
            }
        }
    }

    /// The state passes through native (embedder) nodes only; JavaScript stops it.
    fn mark(&mut self, visited: &mut [bool], native: u8, v: usize, st: u8, queue: &mut Vec<usize>) {
        if visited[v] {
            return;
        }
        visited[v] = true;
        if self.ntype[v] == native {
            self.state[v] = st;
            queue.push(v);
        }
    }

    /// Groups nodes like DevTools' Summary: objects by constructor (and its location), closures by
    /// function and location, detached DOM apart, the rest by node type.
    fn build_classes(&mut self) {
        let n = self.ntype.len();
        let (object, native, closure, hidden, code) = (self.node_type("object"), self.node_type("native"), self.node_type("closure"), self.node_type("hidden"), self.node_type("code"));
        let mut keys: HashMap<(u8, u32, Option<[u32; 3]>), u32> = HashMap::new();
        let mut classes: Vec<Class> = Vec::new();
        let mut class = Vec::with_capacity(n);
        let mut li = 0;
        for v in 0..n {
            while li < self.locations.len() && (self.locations[li][0] as usize) < v {
                li += 1;
            }
            let loc = self.locations.get(li).filter(|l| l[0] as usize == v).map(|l| [l[1], l[2], l[3]]);
            let t = self.ntype[v];
            let detached = t == native && self.state[v] == DETACHED;
            let key = if t == object || t == native {
                (detached as u8, self.name[v], loc)
            } else if t == closure {
                (2, self.name[v], loc)
            } else {
                (3, t as u32, None)
            };
            let c = *keys.entry(key).or_insert_with(|| {
                let raw = self.str(self.name[v]);
                let name = if detached && !raw.starts_with("Detached ") {
                    format!("Detached {raw}")
                } else if t == object || t == native {
                    raw.to_string()
                } else if t == closure {
                    format!("{}()", if raw.is_empty() { "(anonymous)" } else { raw })
                } else if t == hidden {
                    "(system)".into()
                } else if t == code {
                    "(compiled code)".into()
                } else {
                    format!("({})", self.type_name(v))
                };
                classes.push(Class { name, ntype: t, detached, loc: key.2, count: 0, self_size: 0, retained: 0, largest: v as u32 });
                classes.len() as u32 - 1
            });
            class.push(c);
            let k = &mut classes[c as usize];
            k.count += 1;
            k.self_size += self.size[v] as u64;
            if self.retained[v] > self.retained[k.largest as usize] {
                k.largest = v as u32;
            }
        }
        self.class = class;
        let retained = self.exclusive(classes.len(), |v| self.class[v]);
        for (k, r) in classes.iter_mut().zip(retained) {
            k.retained = r;
        }
        self.classes = classes;
    }

    /// Per group, the retained size of its nodes that no other node of the same group dominates
    /// (DevTools' class retained size: nested instances are not counted twice).
    fn exclusive(&self, groups: usize, group: impl Fn(usize) -> u32) -> Vec<u64> {
        let mut out = vec![0u64; groups];
        let mut active = vec![0u32; groups];
        let root = self.root as usize;
        let mut enter = |v: usize, active: &mut Vec<u32>| {
            let g = group(v);
            if g != NONE {
                if active[g as usize] == 0 {
                    out[g as usize] += self.retained[v];
                }
                active[g as usize] += 1;
            }
            g
        };
        let mut stack: Vec<(u32, u32, u32)> = vec![(root as u32, self.dom_first[root], enter(root, &mut active))];
        while let Some(&(v, k, g)) = stack.last() {
            if k < self.dom_first[v as usize + 1] {
                stack.last_mut().unwrap().1 += 1;
                let c = self.dom_kids[k as usize] as usize;
                let gc = enter(c, &mut active);
                stack.push((c as u32, self.dom_first[c], gc));
            } else {
                if g != NONE {
                    active[g as usize] -= 1;
                }
                stack.pop();
            }
        }
        out
    }

    /// The shortest path from the root to `v`, root side first, each step the edge into a node and the
    /// node; null when only weak edges reach it.
    fn path(&self, v: usize, reveal: bool) -> Value {
        let (element, hidden) = (self.edge_type("element"), self.edge_type("hidden"));
        let mut steps = Vec::new();
        let mut at = v;
        while at != self.root as usize {
            let e = self.parent[at];
            if e == NONE {
                return Value::Null;
            }
            let e = e as usize;
            let t = self.etype[e];
            let edge = if t == element || t == hidden { json!(self.ename[e]) } else { json!(self.str(self.ename[e])) };
            steps.push(json!({ "edge_type": self.edge_types.get(t as usize), "edge": edge, "type": self.type_name(at), "name": self.node_name(at, reveal), "id": self.id[at] }));
            at = self.owner(e);
        }
        steps.reverse();
        Value::Array(steps)
    }

    /// A node's name; string contents only with `reveal` (SPEC rule 9).
    fn node_name(&self, v: usize, reveal: bool) -> String {
        let t = self.type_name(v);
        if !reveal && matches!(t, "string" | "concatenated string" | "sliced string") {
            return format!("({t})");
        }
        self.str(self.name[v]).to_string()
    }

    fn loc_json(&self, loc: Option<[u32; 3]>) -> Value {
        match loc {
            Some([sid, line, col]) => json!({ "script_id": sid, "url": self.scripts.get(&sid).map(String::as_str).unwrap_or(""), "line": line + 1, "column": col + 1 }),
            None => Value::Null,
        }
    }

    /// Identity of a class across snapshots: name and location (by URL; script ids differ between
    /// processes). `portable` keys snapshots of two launches or builds: the origin (each variant has its
    /// own port), the bundler's content hash in chunk names and line:column all differ there, so the key
    /// is the name and the script's path without them.
    fn class_key(&self, c: usize, portable: bool) -> String {
        let k = &self.classes[c];
        match (k.loc, portable) {
            (Some([sid, _, _]), true) => format!("{}\u{1}{}", portable_name(&k.name), self.scripts.get(&sid).map(|u| portable_url(u)).unwrap_or_default()),
            (None, true) => portable_name(&k.name),
            (Some([sid, line, col]), false) => match self.scripts.get(&sid) {
                Some(url) => format!("{}\u{1}{url}:{line}:{col}", k.name),
                None => format!("{}\u{1}#{sid}:{line}:{col}", k.name),
            },
            (None, _) => k.name.clone(),
        }
    }

    fn class_json(&self, c: usize, reveal: bool) -> Value {
        let k = &self.classes[c];
        let l = k.largest as usize;
        json!({
            "class": k.name, "type": self.node_types.get(k.ntype as usize), "location": self.loc_json(k.loc),
            "count": k.count, "self_size": k.self_size, "retained_size": k.retained,
            "largest": { "id": self.id[l], "self_size": self.size[l], "retained_size": self.retained[l], "path": self.path(l, reveal) },
        })
    }

    /// Up to three allocation stacks (leaf first) per class, by self size; only when the snapshot
    /// was taken with allocation tracking.
    fn allocations(&self, classes: &[usize]) -> HashMap<usize, Value> {
        let mut out = HashMap::new();
        if self.trace.is_empty() {
            return out;
        }
        let wanted: std::collections::HashSet<u32> = classes.iter().map(|&c| c as u32).collect();
        let mut sites: HashMap<(u32, u32), (u64, u64)> = HashMap::new();
        for v in 0..self.ntype.len() {
            if self.trace[v] != 0 && wanted.contains(&self.class[v]) {
                let e = sites.entry((self.class[v], self.trace[v])).or_default();
                e.0 += 1;
                e.1 += self.size[v] as u64;
            }
        }
        let mut by_class: HashMap<u32, Vec<(u32, u64, u64)>> = HashMap::new();
        for ((c, t), (count, size)) in sites {
            by_class.entry(c).or_default().push((t, count, size));
        }
        for (c, mut v) in by_class {
            v.sort_by_key(|a| std::cmp::Reverse(a.2));
            let stacks: Vec<Value> = v.iter().take(3).map(|&(t, count, size)| json!({ "count": count, "self_size": size, "stack": self.stack(t) })).collect();
            out.insert(c as usize, Value::Array(stacks));
        }
        out
    }

    fn stack(&self, mut t: u32) -> Vec<Value> {
        let mut frames = Vec::new();
        while let Some(&(fi, parent)) = self.trace_nodes.get(&t) {
            if let Some(f) = self.trace_infos.get(fi as usize) {
                let name = self.str(f[1]);
                if name != "(root)" {
                    frames.push(json!({ "name": name, "url": self.str(f[2]), "script_id": f[3], "line": f[4], "column": f[5] }));
                }
            }
            t = parent;
        }
        frames
    }

    /// The biggest owners: nodes by retained size where the dominator tree fans out, so a chain of
    /// single owners (global → array → its elements) shows once, at the node that holds many children.
    fn largest(&self, top: usize, reveal: bool) -> Vec<Value> {
        let synthetic = self.node_type("synthetic");
        let root = self.root as usize;
        let mut cand: Vec<(u64, u32)> = (0..self.ntype.len())
            .filter(|&v| v != root && self.ntype[v] != synthetic && self.retained[v] > 0)
            .filter(|&v| {
                let kids = &self.dom_kids[self.dom_first[v] as usize..self.dom_first[v + 1] as usize];
                let max = kids.iter().map(|&k| self.retained[k as usize]).max().unwrap_or(0);
                max * 10 < self.retained[v] * 9
            })
            .map(|v| (self.retained[v], v as u32))
            .collect();
        if top == 0 {
            return Vec::new();
        }
        if cand.len() > top {
            cand.select_nth_unstable_by(top - 1, |a, b| b.0.cmp(&a.0));
            cand.truncate(top);
        }
        cand.sort_by_key(|a| std::cmp::Reverse(a.0));
        cand.iter()
            .map(|&(r, v)| {
                let v = v as usize;
                let k = &self.classes[self.class[v] as usize];
                json!({ "id": self.id[v], "class": k.name, "type": self.type_name(v), "name": self.node_name(v, reveal), "location": self.loc_json(k.loc), "self_size": self.size[v], "retained_size": r, "path": self.path(v, reveal) })
            })
            .collect()
    }

    /// Classes by retained size, without the synthetic root structure.
    fn by_retained(&self) -> Vec<usize> {
        let synthetic = self.node_type("synthetic");
        let mut order: Vec<usize> = (0..self.classes.len()).filter(|&c| self.classes[c].ntype != synthetic).collect();
        order.sort_by(|&a, &b| self.classes[b].retained.cmp(&self.classes[a].retained).then(self.classes[b].self_size.cmp(&self.classes[a].self_size)));
        order
    }
}

fn intersect(dom: &[u32], mut a: u32, mut b: u32) -> u32 {
    while a != b {
        while a < b {
            a = dom[a as usize];
        }
        while b < a {
            b = dom[b as usize];
        }
    }
    a
}

/// Classes, types, detached DOM and the biggest owners, each with its shortest retainer path.
/// `top` caps every list; `reveal` shows string contents.
pub fn summary(s: &Snapshot, top: usize, reveal: bool) -> Value {
    let n = s.ntype.len();
    let root = s.root as usize;
    let mut types = vec![(0u64, 0u64); s.node_types.len()];
    let (mut unreachable, mut unreachable_size, mut total) = (0u64, 0u64, 0u64);
    for v in 0..n {
        let size = s.size[v] as u64;
        total += size;
        if let Some(t) = types.get_mut(s.ntype[v] as usize) {
            t.0 += 1;
            t.1 += size;
        }
        if s.parent[v] == NONE && v != root {
            unreachable += 1;
            unreachable_size += size;
        }
    }
    let mut types: Vec<(usize, (u64, u64))> = types.into_iter().enumerate().filter(|t| t.1.0 > 0).collect();
    types.sort_by_key(|t| std::cmp::Reverse(t.1.1));
    let order = s.by_retained();
    let shown = &order[..top.min(order.len())];
    let mut allocs = s.allocations(shown);
    let classes: Vec<Value> = shown
        .iter()
        .map(|&c| {
            let mut j = s.class_json(c, reveal);
            if let Some(a) = allocs.remove(&c) {
                j["allocations"] = a;
            }
            j
        })
        .collect();
    let det: Vec<usize> = order.iter().copied().filter(|&c| s.classes[c].detached).collect();
    let det_retained = if det.is_empty() { 0 } else { s.exclusive(1, |v| if s.classes[s.class[v] as usize].detached { 0 } else { NONE })[0] };
    json!({
        "nodes": n,
        "edges": s.etype.len(),
        "self_size": total,
        "unreachable": { "count": unreachable, "self_size": unreachable_size },
        "has_locations": !s.locations.is_empty(),
        "has_allocation_traces": !s.trace.is_empty(),
        "detachedness": if s.has_detachedness { "field" } else { "names" },
        "types": types.iter().map(|(t, (c, sz))| json!({ "type": s.node_types[*t], "count": c, "self_size": sz })).collect::<Vec<_>>(),
        "classes_total": order.len(),
        "classes": classes,
        "detached": {
            "count": det.iter().map(|&c| s.classes[c].count).sum::<u64>(),
            "self_size": det.iter().map(|&c| s.classes[c].self_size).sum::<u64>(),
            "retained_size": det_retained,
            "classes": det.iter().take(top).map(|&c| s.class_json(c, reveal)).collect::<Vec<_>>(),
        },
        "largest": s.largest(top, reveal),
    })
}

/// Per-class count, self and retained deltas from `base` to `head`, by |retained delta|, and the
/// objects only `head` has: by node id when both come from one process (ids are stable within one
/// V8 heap), else the classes `base` lacks.
pub fn diff(base: &Snapshot, head: &Snapshot, same_process: bool, top: usize, reveal: bool) -> Value {
    // Several classes can share a portable key (same name in one chunk): their stats add up.
    let index = |s: &Snapshot| -> HashMap<String, Vec<usize>> {
        let mut m: HashMap<String, Vec<usize>> = HashMap::new();
        for c in 0..s.classes.len() {
            m.entry(s.class_key(c, !same_process)).or_default().push(c);
        }
        m
    };
    let (bi, hi) = (index(base), index(head));
    let keys: Vec<&String> = hi.keys().chain(bi.keys().filter(|k| !hi.contains_key(*k))).collect();
    let stats = |s: &Snapshot, cs: Option<&Vec<usize>>| {
        cs.into_iter().flatten().map(|&c| &s.classes[c]).fold((0i64, 0i64, 0i64), |a, k| (a.0 + k.count as i64, a.1 + k.self_size as i64, a.2 + k.retained as i64))
    };
    let mut rows: Vec<(i64, i64, i64, Value)> = keys
        .into_iter()
        .filter_map(|key| {
            let (bcs, hcs) = (bi.get(key), hi.get(key));
            let ((bc, bs, br), (hc, hs, hr)) = (stats(base, bcs), stats(head, hcs));
            let (b, h) = (bcs.and_then(|v| v.first()).map(|&c| &base.classes[c]), hcs.and_then(|v| v.first()).map(|&c| &head.classes[c]));
            if (bc, bs, br) == (hc, hs, hr) {
                return None;
            }
            let (s, k) = match (h, b) {
                (Some(k), _) => (head, k),
                (None, Some(k)) => (base, k),
                (None, None) => return None,
            };
            if s.node_types.get(k.ntype as usize).is_some_and(|t| t == "synthetic") {
                return None;
            }
            let row = json!({
                "class": if same_process { k.name.clone() } else { portable_name(&k.name) }, "type": s.node_types.get(k.ntype as usize), "location": s.loc_json(k.loc),
                "base": { "count": bc, "self_size": bs, "retained_size": br },
                "head": { "count": hc, "self_size": hs, "retained_size": hr },
                "delta": { "count": hc - bc, "self_size": hs - bs, "retained_size": hr - br },
            });
            Some((hr - br, hs - bs, hc - bc, row))
        })
        .collect();
    rows.sort_by(|a, b| b.0.abs().cmp(&a.0.abs()).then(b.1.abs().cmp(&a.1.abs())).then(b.2.abs().cmp(&a.2.abs())));
    let changed = rows.len();
    let totals = |s: &Snapshot| (s.ntype.len() as i64, s.size.iter().map(|&x| x as i64).sum::<i64>());
    let ((bn, bs), (hn, hs)) = (totals(base), totals(head));
    let new = if same_process {
        let ids = base.sorted_ids();
        born(head, |id| ids.binary_search(&id).is_err(), top, reveal)
    } else {
        let only: Vec<Value> = head.by_retained().into_iter().filter(|&c| !bi.contains_key(&head.class_key(c, true))).take(top).map(|c| head.class_json(c, reveal)).collect();
        json!({ "by": "class", "classes": only })
    };
    json!({
        "same_process": same_process,
        "base": { "nodes": bn, "self_size": bs },
        "head": { "nodes": hn, "self_size": hs },
        "delta": { "nodes": hn - bn, "self_size": hs - bs },
        "classes_changed": changed,
        "classes": rows.into_iter().take(top).map(|r| r.3).collect::<Vec<_>>(),
        "new": new,
    })
}

/// A class name as two launches share it: origins (`Window (global*) / http://127.0.0.1:61610`) become
/// `<origin>`, and an element's attributes (`Detached <button id="menu-cl-1048" …>`, ids that differ per
/// run) are dropped down to its tag.
fn portable_name(name: &str) -> String {
    if let Some(tag) = name.strip_prefix("Detached <").or_else(|| name.strip_prefix('<')) {
        let tag: String = tag.chars().take_while(|c| !c.is_whitespace() && *c != '>').collect();
        return format!("{}<{tag}>", if name.starts_with("Detached") { "Detached " } else { "" });
    }
    let (mut out, mut rest) = (String::new(), name);
    while let Some(i) = rest.find("://") {
        let start = rest[..i].rfind(|c: char| !c.is_ascii_alphabetic()).map_or(0, |j| j + 1);
        out.push_str(&rest[..start]);
        out.push_str("<origin>");
        let after = &rest[i + 3..];
        rest = &after[after.find(|c: char| c == '/' || c.is_whitespace() || c == ')').unwrap_or(after.len())..];
    }
    out + rest
}

/// A script URL as two launches or builds share it: the path without origin, query or the bundler's
/// content hash (`/_assets/route-CN8YftxL.js` → `/_assets/route.js`).
fn portable_url(url: &str) -> String {
    let path = url.split_once("://").map(|(_, r)| r.find('/').map_or("", |i| &r[i..])).unwrap_or(url);
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
    // A hash is random: some digit or capital, never a plain word like `my-component`.
    let random = |h: &str| h.chars().any(|c| c.is_ascii_digit() || c.is_ascii_uppercase());
    let unhashed = |stem: &str| -> Option<String> {
        // Vite and Rollup: `-` and 8 base64url characters, which may themselves hold `-` or `_`.
        if let Some(h) = stem
            .get(stem.len().saturating_sub(8)..)
            .filter(|h| stem.len() > 9 && stem.as_bytes()[stem.len() - 9] == b'-' && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && random(h))
        {
            return Some(stem[..stem.len() - h.len() - 1].to_string());
        }
        let (name, hash) = stem.rsplit_once('-')?;
        (hash.len() >= 6 && hash.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && random(hash)).then(|| name.to_string())
    };
    let file = match file.rsplit_once('.') {
        Some((stem, ext)) => unhashed(stem).map_or_else(|| file.to_string(), |name| format!("{name}.{ext}")),
        None => file.to_string(),
    };
    format!("{dir}/{file}")
}

/// Leak suspects (SPEC §8.6): objects allocated between S0 and S1 that are still alive in S2,
/// grouped by class in S2 with retainer paths. Takes S0's and S1's sorted ids (`ids`).
pub fn leaks(s0: &[u32], s1: &[u32], s2: &Snapshot, top: usize, reveal: bool) -> Value {
    born(s2, |id| s1.binary_search(&id).is_ok() && s0.binary_search(&id).is_err(), top, reveal)
}

/// The nodes of `s` whose id passes `is_new`, by class: count, self size and the retained size no
/// other new node of the class already holds; totals count what new nodes retain without overlap.
fn born(s: &Snapshot, is_new: impl Fn(u32) -> bool, top: usize, reveal: bool) -> Value {
    let n = s.ntype.len();
    let flag: Vec<bool> = s.id.iter().map(|&id| is_new(id)).collect();
    let mut stats = vec![(0u64, 0u64, NONE); s.classes.len()];
    for v in (0..n).filter(|&v| flag[v]) {
        let st = &mut stats[s.class[v] as usize];
        st.0 += 1;
        st.1 += s.size[v] as u64;
        if st.2 == NONE || s.retained[v] > s.retained[st.2 as usize] {
            st.2 = v as u32;
        }
    }
    let retained = s.exclusive(s.classes.len(), |v| if flag[v] { s.class[v] } else { NONE });
    let total = s.exclusive(1, |v| if flag[v] { 0 } else { NONE })[0];
    let synthetic = s.node_type("synthetic");
    let mut order: Vec<usize> = (0..s.classes.len()).filter(|&c| stats[c].0 > 0 && s.classes[c].ntype != synthetic).collect();
    order.sort_by(|&a, &b| retained[b].cmp(&retained[a]).then(stats[b].1.cmp(&stats[a].1)));
    let classes: Vec<Value> = order
        .iter()
        .take(top)
        .map(|&c| {
            let k = &s.classes[c];
            let l = stats[c].2 as usize;
            json!({
                "class": k.name, "type": s.node_types.get(k.ntype as usize), "location": s.loc_json(k.loc),
                "count": stats[c].0, "self_size": stats[c].1, "retained_size": retained[c],
                "largest": { "id": s.id[l], "self_size": s.size[l], "retained_size": s.retained[l], "path": s.path(l, reveal) },
            })
        })
        .collect();
    json!({
        "by": "id",
        "count": stats.iter().map(|x| x.0).sum::<u64>(),
        "self_size": stats.iter().map(|x| x.1).sum::<u64>(),
        "retained_size": total,
        "classes_total": order.len(),
        "classes": classes,
    })
}

pub fn summary_text(v: &Value, top: usize) -> String {
    let mut t = format!(
        "heap {} · {} objects · {} edges · unreachable {} ({})\n",
        bytes(v["self_size"].as_f64().unwrap_or(0.0)),
        v["nodes"],
        v["edges"],
        v["unreachable"]["count"],
        bytes(v["unreachable"]["self_size"].as_f64().unwrap_or(0.0)),
    );
    t += &format!("  {:>10} {:>10} {:>9}  class (by retained size; {} classes)\n", "retained", "self", "count", v["classes_total"]);
    t += &class_rows(&v["classes"], top);
    let d = &v["detached"];
    if d["count"].as_u64().unwrap_or(0) > 0 {
        t += &format!("detached DOM · {} nodes · {} self · {} retained\n", d["count"], bytes(d["self_size"].as_f64().unwrap_or(0.0)), bytes(d["retained_size"].as_f64().unwrap_or(0.0)));
        t += &class_rows(&d["classes"], top.min(5));
    }
    t += "largest owners (where the dominator tree fans out)\n";
    for o in v["largest"].as_array().into_iter().flatten().take(top.min(10)) {
        t += &format!("  {:>10}  {} @{}{}\n", bytes(o["retained_size"].as_f64().unwrap_or(0.0)), o["class"].as_str().unwrap_or(""), o["id"], location(&o["location"]));
        t += &format!("  {:>10}    ↳ {}\n", "", path_text(&o["path"]));
    }
    t
}

pub fn diff_text(v: &Value, top: usize) -> String {
    let d = &v["delta"];
    let mut t = format!(
        "heap diff · {} self · {:+} objects ({} → {})\n",
        signed_bytes(d["self_size"].as_f64().unwrap_or(0.0)),
        d["nodes"].as_i64().unwrap_or(0),
        bytes(v["base"]["self_size"].as_f64().unwrap_or(0.0)),
        bytes(v["head"]["self_size"].as_f64().unwrap_or(0.0)),
    );
    t += &format!("  {:>10} {:>10} {:>9}  class (by |retained delta|; {} changed)\n", "Δretained", "Δself", "Δcount", v["classes_changed"]);
    for r in v["classes"].as_array().into_iter().flatten().take(top) {
        let d = &r["delta"];
        t += &format!(
            "  {:>10} {:>10} {:>+9}  {}{}\n",
            signed_bytes(d["retained_size"].as_f64().unwrap_or(0.0)),
            signed_bytes(d["self_size"].as_f64().unwrap_or(0.0)),
            d["count"].as_i64().unwrap_or(0),
            r["class"].as_str().unwrap_or(""),
            location(&r["location"])
        );
    }
    let new = &v["new"];
    if new["by"] == "id" {
        t += &format!("new in head (by node id) · {}\n", born_text(new, top));
    } else {
        t += "classes only in head\n";
        t += &class_rows(&new["classes"], top);
    }
    t
}

pub fn leaks_text(v: &Value, top: usize) -> String {
    format!("allocated between S0 and S1, alive in S2 · {}", born_text(v, top))
}

fn born_text(v: &Value, top: usize) -> String {
    format!("{} objects · {} self · {} retained\n{}", v["count"], bytes(v["self_size"].as_f64().unwrap_or(0.0)), bytes(v["retained_size"].as_f64().unwrap_or(0.0)), class_rows(&v["classes"], top))
}

fn class_rows(rows: &Value, top: usize) -> String {
    let mut t = String::new();
    for r in rows.as_array().into_iter().flatten().take(top) {
        t += &format!(
            "  {:>10} {:>10} {:>9}  {}{}\n",
            bytes(r["retained_size"].as_f64().unwrap_or(0.0)),
            bytes(r["self_size"].as_f64().unwrap_or(0.0)),
            r["count"].as_u64().unwrap_or(0),
            r["class"].as_str().unwrap_or(""),
            location(&r["location"])
        );
        if let Some(p) = r["largest"]["path"].as_array().filter(|p| !p.is_empty()) {
            t += &format!("  {:>32}↳ {}\n", "", path_text(&Value::Array(p.clone())));
        }
        if let Some(a) = r["allocations"].get(0) {
            let frames: Vec<String> = a["stack"]
                .as_array()
                .into_iter()
                .flatten()
                .take(3)
                .map(|f| {
                    format!(
                        "{} {}",
                        f["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("(anonymous)"),
                        location(&json!({ "url": f["url"], "script_id": f["script_id"], "line": f["line"], "column": f["column"] })).trim()
                    )
                })
                .collect();
            t += &format!("  {:>32}allocated ×{} at {}\n", "", a["count"], frames.join(" ← "));
        }
    }
    t
}

fn location(l: &Value) -> String {
    if l.is_null() || (l["script_id"] == 0 && l["url"] == "") {
        return String::new();
    }
    let url = match l["url"].as_str().unwrap_or("") {
        "" => format!("script {}", l["script_id"]),
        u if u.chars().count() > 60 => format!("…{}", u.chars().skip(u.chars().count() - 59).collect::<String>()),
        u => u.to_string(),
    };
    format!("  {url}:{}:{}", l["line"], l["column"])
}

/// A retainer path on one line: `Window › .cache Map › [3] Foo`; long paths keep both ends.
fn path_text(p: &Value) -> String {
    let Some(steps) = p.as_array() else { return "(only weakly reachable)".into() };
    let short = |s: &str| -> String { if s.chars().count() > 48 { s.chars().take(47).chain(['…']).collect() } else { s.to_string() } };
    let part = |s: &Value| {
        let name = s["name"].as_str().unwrap_or("");
        let name = if name.is_empty() { format!("({})", s["type"].as_str().unwrap_or("")) } else { short(name) };
        let edge = match (s["edge_type"].as_str().unwrap_or(""), &s["edge"]) {
            ("element", e) => format!("[{e}] "),
            ("hidden", e) => format!("<{e}> "),
            ("context", e) => format!("::{} ", short(e.as_str().unwrap_or(""))),
            (_, e) => format!(".{} ", short(e.as_str().unwrap_or(""))),
        };
        format!("{edge}{name}")
    };
    let mut parts: Vec<String> = steps.iter().map(part).collect();
    if let Some(first) = steps.first() {
        parts[0] = first["name"].as_str().map(|n| n.chars().take(48).collect()).unwrap_or_default();
    }
    if parts.len() > 7 {
        let tail = parts.split_off(parts.len() - 4);
        parts.truncate(2);
        parts.push("…".into());
        parts.extend(tail);
    }
    parts.join(" › ")
}

fn bytes(v: f64) -> String {
    let a = v.abs();
    if a >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", v / (1024.0 * 1024.0 * 1024.0))
    } else if a >= 1024.0 * 1024.0 {
        format!("{:.1} MB", v / (1024.0 * 1024.0))
    } else if a >= 1024.0 {
        format!("{:.0} KB", v / 1024.0)
    } else {
        format!("{v:.0} B")
    }
}

fn signed_bytes(v: f64) -> String {
    if v > 0.0 {
        format!("+{}", bytes(v))
    } else if v < 0.0 {
        format!("−{}", bytes(-v))
    } else {
        "0 B".into()
    }
}

fn parse(path: &Path) -> Result<Snapshot> {
    let mut file = std::fs::File::open(path)?;
    let mut magic = [0u8; 2];
    let gz = file.read_exact(&mut magic).is_ok() && magic == [0x1f, 0x8b];
    file.seek(SeekFrom::Start(0))?;
    let src: Box<dyn Read> = if gz { Box::new(flate2::read::MultiGzDecoder::new(std::io::BufReader::with_capacity(1 << 20, file))) } else { Box::new(file) };
    let mut sc = Scan { src, buf: vec![0; 1 << 20], pos: 0, len: 0 };
    document(&mut sc).map_err(|e| Error::msg(format!("{}: {}", path.display(), e.message)))
}

fn document(sc: &mut Scan) -> Result<Snapshot> {
    let mut s = Snapshot::default();
    let (mut nf, mut ncol, mut ecol) = (0usize, Vec::new(), Vec::new());
    let (mut info_fields, mut tree_fields, mut loc_fields) = (Vec::new(), Vec::new(), Vec::new());
    let (mut counts, mut raw_locs, mut raw_infos) = (Vec::<u32>::new(), Vec::<u32>::new(), Vec::<u32>::new());
    let mut tree = Value::Null;
    let mut key = Vec::new();
    sc.expect(b'{')?;
    loop {
        match sc.ws()? {
            b'}' => break,
            b',' => {
                sc.bump();
                continue;
            }
            _ => {}
        }
        key.clear();
        sc.string(&mut key)?;
        sc.expect(b':')?;
        match key.as_slice() {
            b"snapshot" => {
                let mut raw = Vec::new();
                sc.value(Some(&mut raw))?;
                let v: Value = serde_json::from_slice(&raw)?;
                let meta = &v["meta"];
                let names = |x: &Value| -> Vec<String> { x.as_array().into_iter().flatten().map(|f| f.as_str().unwrap_or("").to_string()).collect() };
                let node_fields = names(&meta["node_fields"]);
                nf = node_fields.len();
                ncol = node_fields
                    .iter()
                    .map(|f| match f.as_str() {
                        "type" => 0u8,
                        "name" => 1,
                        "id" => 2,
                        "self_size" => 3,
                        "edge_count" => 4,
                        "trace_node_id" => 5,
                        "detachedness" => 6,
                        _ => 9,
                    })
                    .collect();
                ecol = names(&meta["edge_fields"])
                    .iter()
                    .map(|f| match f.as_str() {
                        "type" => 0u8,
                        "name_or_index" => 1,
                        "to_node" => 2,
                        _ => 9,
                    })
                    .collect();
                if !ncol.contains(&4) || !ecol.contains(&2) {
                    return Err(Error::msg("heap snapshot: meta lacks edge_count or to_node"));
                }
                s.has_detachedness = ncol.contains(&6);
                s.node_types = names(&meta["node_types"][0]);
                s.edge_types = names(&meta["edge_types"][0]);
                info_fields = names(&meta["trace_function_info_fields"]);
                tree_fields = names(&meta["trace_node_fields"]);
                loc_fields = names(&meta["location_fields"]);
                s.root = (v["root_index"].as_u64().unwrap_or(0) / nf as u64) as u32;
                let n = v["node_count"].as_u64().unwrap_or(0) as usize;
                let e = v["edge_count"].as_u64().unwrap_or(0) as usize;
                for col in [&mut s.name, &mut s.id, &mut s.size, &mut counts] {
                    col.reserve(n);
                }
                s.ntype.reserve(n);
                s.etype.reserve(e);
                s.ename.reserve(e);
                s.eto.reserve(e);
            }
            b"nodes" => {
                if nf == 0 {
                    return Err(Error::msg("heap snapshot: nodes before snapshot.meta"));
                }
                let mut i = 0;
                sc.ints(|x| {
                    match ncol[i] {
                        0 => s.ntype.push(x as u8),
                        1 => s.name.push(x as u32),
                        2 => s.id.push(x as u32),
                        3 => s.size.push(x.min(u32::MAX as u64) as u32),
                        4 => counts.push(x as u32),
                        5 => s.trace.push(x as u32),
                        6 => s.state.push(x as u8),
                        _ => {}
                    }
                    i = if i + 1 == nf { 0 } else { i + 1 };
                })?;
            }
            b"edges" => {
                if nf == 0 {
                    return Err(Error::msg("heap snapshot: edges before snapshot.meta"));
                }
                let (ef, mut i) = (ecol.len(), 0);
                sc.ints(|x| {
                    match ecol[i] {
                        0 => s.etype.push(x as u8),
                        1 => s.ename.push(x as u32),
                        2 => s.eto.push((x / nf as u64) as u32),
                        _ => {}
                    }
                    i = if i + 1 == ef { 0 } else { i + 1 };
                })?;
            }
            b"strings" => {
                sc.expect(b'[')?;
                let mut buf = Vec::new();
                loop {
                    match sc.ws()? {
                        b']' => {
                            sc.bump();
                            break;
                        }
                        b',' => sc.bump(),
                        _ => {
                            buf.clear();
                            sc.string(&mut buf)?;
                            s.strings.push_str(&String::from_utf8_lossy(&buf));
                            s.string_ends.push(s.strings.len());
                        }
                    }
                }
            }
            b"locations" => sc.ints(|x| raw_locs.push(x as u32))?,
            b"trace_function_infos" => sc.ints(|x| raw_infos.push(x as u32))?,
            b"trace_tree" => {
                let mut raw = Vec::new();
                sc.value(Some(&mut raw))?;
                tree = serde_json::from_slice(&raw)?;
            }
            _ => sc.value(None)?,
        }
    }
    let n = s.ntype.len();
    if n == 0 || s.name.len() != n || s.id.len() != n || s.size.len() != n || counts.len() != n {
        return Err(Error::msg("heap snapshot: missing or uneven node fields"));
    }
    s.first_edge = Vec::with_capacity(n + 1);
    s.first_edge.push(0);
    let mut acc = 0u64;
    for c in counts {
        acc += c as u64;
        s.first_edge.push(acc as u32);
    }
    let e = s.eto.len();
    if acc != e as u64 || s.etype.len() != e || s.ename.len() != e || s.eto.iter().any(|&t| t as usize >= n) || s.root as usize >= n {
        return Err(Error::msg(format!("heap snapshot: {e} edges, but nodes count {acc}, or an edge points outside the nodes")));
    }
    if s.state.len() != n {
        s.state = vec![0; n];
    }
    let at = |fields: &[String], name: &str, default: usize| fields.iter().position(|f| f == name).unwrap_or(default);
    let lf = loc_fields.len().max(4);
    let (lo, ls, ll, lc) = (at(&loc_fields, "object_index", 0), at(&loc_fields, "script_id", 1), at(&loc_fields, "line", 2), at(&loc_fields, "column", 3));
    s.locations = raw_locs.chunks_exact(lf).map(|l| [l[lo] / nf as u32, l[ls], l[ll], l[lc]]).filter(|l| (l[0] as usize) < n).collect();
    s.locations.sort_unstable_by_key(|l| l[0]);
    let fi = info_fields.len().max(6);
    let cols = ["function_id", "name", "script_name", "script_id", "line", "column"].map(|f| at(&info_fields, f, usize::MAX));
    s.trace_infos = raw_infos.chunks_exact(fi).map(|f| cols.map(|c| f.get(c).copied().unwrap_or(0))).collect();
    let tf = (at(&tree_fields, "id", 0), at(&tree_fields, "function_info_index", 1), at(&tree_fields, "children", 4), tree_fields.len().max(5));
    walk_tree(&tree, tf, NONE, &mut s.trace_nodes);
    if s.trace.len() != n || s.trace_infos.is_empty() {
        s.trace = Vec::new();
    }
    Ok(s)
}

/// `trace_tree`: [id, function_info_index, count, size, [children…]] repeated, nested.
fn walk_tree(v: &Value, f: (usize, usize, usize, usize), parent: u32, out: &mut HashMap<u32, (u32, u32)>) {
    let Some(a) = v.as_array() else { return };
    for node in a.chunks_exact(f.3) {
        let id = node[f.0].as_u64().unwrap_or(0) as u32;
        out.insert(id, (node[f.1].as_u64().unwrap_or(0) as u32, parent));
        walk_tree(&node[f.2], f, id, out);
    }
}

/// A byte scanner over the decompressed JSON.
struct Scan {
    src: Box<dyn Read>,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
}

impl Scan {
    fn fill(&mut self) -> Result<()> {
        self.pos = 0;
        self.len = loop {
            match self.src.read(&mut self.buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        };
        Ok(())
    }

    /// The next byte without consuming it; 0 at the end.
    fn peek(&mut self) -> Result<u8> {
        if self.pos == self.len {
            self.fill()?;
        }
        Ok(if self.pos < self.len { self.buf[self.pos] } else { 0 })
    }

    fn bump(&mut self) {
        self.pos += 1;
    }

    fn byte(&mut self) -> Result<u8> {
        match self.peek()? {
            0 => Err(Error::msg("heap snapshot: truncated")),
            c => {
                self.bump();
                Ok(c)
            }
        }
    }

    /// The next byte after whitespace, not consumed; errors at the end.
    fn ws(&mut self) -> Result<u8> {
        loop {
            match self.peek()? {
                b' ' | b'\n' | b'\r' | b'\t' => self.bump(),
                0 => return Err(Error::msg("heap snapshot: truncated")),
                c => return Ok(c),
            }
        }
    }

    fn expect(&mut self, c: u8) -> Result<()> {
        let got = self.ws()?;
        if got != c {
            return Err(Error::msg(format!("heap snapshot: expected '{}', found '{}'", c as char, got as char)));
        }
        self.bump();
        Ok(())
    }

    /// A flat array of non-negative integers, straight from the buffer.
    fn ints(&mut self, mut f: impl FnMut(u64)) -> Result<()> {
        self.expect(b'[')?;
        let (mut n, mut in_num) = (0u64, false);
        loop {
            if self.pos == self.len {
                self.fill()?;
                if self.len == 0 {
                    return Err(Error::msg("heap snapshot: truncated"));
                }
            }
            let start = self.pos;
            let mut end = None;
            for (i, &c) in self.buf[start..self.len].iter().enumerate() {
                match c {
                    b'0'..=b'9' => {
                        n = n * 10 + (c - b'0') as u64;
                        in_num = true;
                    }
                    b',' | b' ' | b'\n' | b'\r' | b'\t' | b']' => {
                        if in_num {
                            f(n);
                            n = 0;
                            in_num = false;
                        }
                        if c == b']' {
                            end = Some(start + i + 1);
                            break;
                        }
                    }
                    _ => return Err(Error::msg(format!("heap snapshot: unexpected '{}' in an integer array", c as char))),
                }
            }
            if let Some(p) = end {
                self.pos = p;
                return Ok(());
            }
            self.pos = self.len;
        }
    }

    /// A JSON string, decoded into UTF-8 bytes.
    fn string(&mut self, out: &mut Vec<u8>) -> Result<()> {
        self.expect(b'"')?;
        loop {
            if self.pos == self.len {
                self.fill()?;
                if self.len == 0 {
                    return Err(Error::msg("heap snapshot: truncated"));
                }
            }
            let chunk = &self.buf[self.pos..self.len];
            match chunk.iter().position(|&c| c == b'"' || c == b'\\') {
                None => {
                    out.extend_from_slice(chunk);
                    self.pos = self.len;
                }
                Some(i) => {
                    out.extend_from_slice(&chunk[..i]);
                    let quote = chunk[i] == b'"';
                    self.pos += i + 1;
                    if quote {
                        return Ok(());
                    }
                    let c = self.byte()?;
                    self.escape(c, out)?;
                }
            }
        }
    }

    fn escape(&mut self, c: u8, out: &mut Vec<u8>) -> Result<()> {
        let ch = match c {
            b'u' => {
                let hi = self.hex4()?;
                let mut code = hi;
                // A surrogate pair arrives as two escapes; a lone surrogate becomes U+FFFD.
                if (0xD800..0xDC00).contains(&hi) && self.peek()? == b'\\' {
                    self.bump();
                    let c2 = self.byte()?;
                    if c2 != b'u' {
                        out.extend_from_slice("\u{FFFD}".as_bytes());
                        return self.escape(c2, out);
                    }
                    let lo = self.hex4()?;
                    if (0xDC00..0xE000).contains(&lo) {
                        code = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                    } else {
                        out.extend_from_slice("\u{FFFD}".as_bytes());
                        code = lo;
                    }
                }
                char::from_u32(code).unwrap_or('\u{FFFD}')
            }
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            other => other as char,
        };
        out.extend_from_slice(ch.encode_utf8(&mut [0u8; 4]).as_bytes());
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32> {
        let mut v = 0;
        for _ in 0..4 {
            let d = (self.byte()? as char).to_digit(16).ok_or_else(|| Error::msg("heap snapshot: bad \\u escape"))?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    /// One JSON value, skipped or copied raw into `keep`.
    fn value(&mut self, mut keep: Option<&mut Vec<u8>>) -> Result<()> {
        self.ws()?;
        let (mut depth, mut in_str, mut esc) = (0u32, false, false);
        loop {
            let c = self.peek()?;
            if c == 0 {
                return Err(Error::msg("heap snapshot: truncated"));
            }
            if !in_str && depth == 0 && matches!(c, b',' | b'}' | b']') {
                return Ok(());
            }
            self.bump();
            if let Some(k) = keep.as_deref_mut() {
                k.push(c);
            }
            if in_str {
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == b'"' {
                    in_str = false;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                continue;
            }
            match c {
                b'"' => in_str = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
    }
}
