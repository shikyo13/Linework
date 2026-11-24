use super::source::Source;
use std::{
    io,
    sync::{Arc, OnceLock},
};

pub(crate) type Root = Option<Arc<Node>>;
pub(crate) enum Body {
    Leaf(Arc<Source>, u64),
    Branch(Arc<Node>, Arc<Node>),
}
pub(crate) struct Node {
    pub len: u64,
    pub body: Body,
    height: u8,
    pub(crate) first: u8,
    pub(crate) last: u8,
    breaks: OnceLock<u64>,
    pub(crate) char_count: OnceLock<u64>,
    complete: OnceLock<()>,
}
fn branch(a: Arc<Node>, b: Arc<Node>) -> Arc<Node> {
    Arc::new(Node {
        len: a.len + b.len,
        height: a.height.max(b.height) + 1,
        first: a.first,
        last: b.last,
        body: Body::Branch(a, b),
        breaks: Default::default(),
        char_count: Default::default(),
        complete: Default::default(),
    })
}
fn kids(n: &Node) -> (&Arc<Node>, &Arc<Node>) {
    match &n.body {
        Body::Branch(l, r) => (l, r),
        _ => unreachable!("AVL internal node"),
    }
}
fn balance(a: Arc<Node>, b: Arc<Node>) -> Arc<Node> {
    if a.height > b.height + 1 {
        let (x, y) = kids(&a);
        if x.height >= y.height {
            return branch(x.clone(), branch(y.clone(), b));
        }
        let (yl, yr) = kids(y);
        return branch(branch(x.clone(), yl.clone()), branch(yr.clone(), b));
    }
    if b.height > a.height + 1 {
        let (x, y) = kids(&b);
        if y.height >= x.height {
            return branch(branch(a, x.clone()), y.clone());
        }
        let (xl, xr) = kids(x);
        return branch(branch(a, xl.clone()), branch(xr.clone(), y.clone()));
    }
    branch(a, b)
}
pub(crate) fn join(a: Root, b: Root) -> Root {
    let (a, b) = match (a, b) {
        (Some(x), Some(y)) => (x, y),
        (a, b) => return a.or(b),
    };
    if a.height > b.height + 1 {
        let (l, r) = kids(&a);
        return Some(balance(l.clone(), join(Some(r.clone()), Some(b)).unwrap()));
    }
    if b.height > a.height + 1 {
        let (l, r) = kids(&b);
        return Some(balance(join(Some(a), Some(l.clone())).unwrap(), r.clone()));
    }
    Some(branch(a, b))
}
pub(crate) fn leaf(src: Arc<Source>, at: u64, len: u64) -> io::Result<Root> {
    if len == 0 {
        return Ok(None);
    }
    Ok(Some(Arc::new(Node {
        len,
        height: 1,
        first: src.at(at)?,
        last: src.at(at + len - 1)?,
        body: Body::Leaf(src, at),
        breaks: Default::default(),
        char_count: Default::default(),
        complete: Default::default(),
    })))
}
/// A leaf whose edge bytes and line-break count the caller already knows, so
/// creating it reads nothing from its source. `breaks` counts the piece on its
/// own: a final CR is a break even when the next piece starts with LF.
pub(crate) fn piece(
    src: Arc<Source>,
    at: u64,
    len: u64,
    first: u8,
    last: u8,
    breaks: u64,
) -> Arc<Node> {
    debug_assert!(len != 0);
    Arc::new(Node {
        len,
        height: 1,
        first,
        last,
        body: Body::Leaf(src, at),
        breaks: OnceLock::from(breaks),
        char_count: Default::default(),
        complete: Default::default(),
    })
}
/// A balanced tree over leaves in document order, built in one pass.
pub(crate) fn build(nodes: &[Arc<Node>]) -> Root {
    match nodes {
        [] => None,
        [one] => Some(one.clone()),
        _ => {
            let (a, b) = nodes.split_at(nodes.len().div_ceil(2));
            Some(branch(build(a)?, build(b)?))
        }
    }
}
/// Every leaf in document order.
pub(crate) fn leaves(root: &Root) -> Vec<Arc<Node>> {
    let mut out = Vec::new();
    let mut stack: Vec<&Arc<Node>> = root.iter().collect();
    while let Some(node) = stack.pop() {
        match &node.body {
            Body::Leaf(..) => out.push(node.clone()),
            Body::Branch(left, right) => {
                stack.push(right);
                stack.push(left);
            }
        }
    }
    out
}
pub(crate) fn split(root: Root, pos: u64) -> io::Result<(Root, Root)> {
    let n = match root {
        Some(n) => n,
        None => return Ok((None, None)),
    };
    if pos == 0 {
        return Ok((None, Some(n)));
    }
    if pos == n.len {
        return Ok((Some(n), None));
    }
    match &n.body {
        Body::Leaf(s, at) => Ok((
            leaf(s.clone(), *at, pos)?,
            leaf(s.clone(), at + pos, n.len - pos)?,
        )),
        Body::Branch(l, r) if pos < l.len => {
            let (a, b) = split(Some(l.clone()), pos)?;
            Ok((a, join(b, Some(r.clone()))))
        }
        Body::Branch(l, r) => {
            let (a, b) = split(Some(r.clone()), pos - l.len)?;
            Ok((join(Some(l.clone()), a), b))
        }
    }
}
pub(crate) fn slice(root: Root, at: u64, len: u64) -> io::Result<Root> {
    let (_, tail) = split(root, at)?;
    let (piece, _) = split(tail, len)?;
    Ok(piece)
}
pub(crate) fn replace(root: Root, at: u64, delete: u64, insert: Root) -> io::Result<Root> {
    let (a, b) = split(root, at)?;
    let (_, b) = split(b, delete)?;
    Ok(join(join(a, insert), b))
}
pub(crate) fn same(a: &Root, b: &Root) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}
impl Node {
    pub fn indexed(&self) -> u64 {
        if self.complete.get().is_some() {
            return self.len;
        }
        let n = match &self.body {
            Body::Leaf(s, offset) => s.indexed().saturating_sub(*offset).min(self.len),
            Body::Branch(l, r) => {
                let n = l.indexed();
                if n < l.len {
                    n
                } else {
                    n + r.indexed()
                }
            }
        };
        if n == self.len {
            let _ = self.complete.set(());
        }
        n
    }
    pub fn at(&self, pos: u64) -> io::Result<u8> {
        match &self.body {
            Body::Leaf(s, at) => s.at(at + pos),
            Body::Branch(l, _) if pos < l.len => l.at(pos),
            Body::Branch(l, r) => r.at(pos - l.len),
        }
    }
    pub fn read(&self, pos: u64, out: &mut [u8]) -> io::Result<()> {
        if out.is_empty() {
            return Ok(());
        }
        match &self.body {
            Body::Leaf(s, at) => s.read(at + pos, out),
            Body::Branch(l, r) if pos < l.len => {
                let n = (l.len - pos).min(out.len() as u64) as usize;
                l.read(pos, &mut out[..n])?;
                r.read(0, &mut out[n..])
            }
            Body::Branch(l, r) => r.read(pos - l.len, out),
        }
    }
    /// Like `read`, with the caller choosing how each source is read.
    pub fn read_via(
        &self,
        pos: u64,
        out: &mut [u8],
        read: &mut impl FnMut(&Arc<Source>, u64, &mut [u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        if out.is_empty() {
            return Ok(());
        }
        match &self.body {
            Body::Leaf(s, at) => read(s, at + pos, out),
            Body::Branch(l, r) if pos < l.len => {
                let n = (l.len - pos).min(out.len() as u64) as usize;
                l.read_via(pos, &mut out[..n], read)?;
                r.read_via(0, &mut out[n..], read)
            }
            Body::Branch(l, r) => r.read_via(pos - l.len, out, read),
        }
    }
    fn prefix(&self, n: u64) -> io::Result<u64> {
        if n == 0 {
            return Ok(0);
        }
        if n == self.len {
            if let Some(&c) = self.breaks.get() {
                return Ok(c);
            }
        }
        let count = match &self.body {
            Body::Leaf(s, at) => {
                let end = at + n;
                let seam = end < s.len && s.at(end - 1)? == 13 && s.at(end)? == 10;
                s.rank(end)? - s.rank(*at)? + seam as u64
            }
            Body::Branch(l, _) if n <= l.len => l.prefix(n)?,
            Body::Branch(l, r) => {
                l.prefix(l.len)? + r.prefix(n - l.len)? - u64::from(l.last == 13 && r.first == 10)
            }
        };
        if n == self.len {
            let _ = self.breaks.set(count);
        }
        Ok(count)
    }
    pub fn line(&self, pos: u64) -> io::Result<u64> {
        let n = self.prefix(pos)?;
        Ok(n - u64::from(
            pos > 0 && pos < self.len && self.at(pos - 1)? == 13 && self.at(pos)? == 10,
        ))
    }
    pub fn start(&self, n: u64, upto: u64) -> io::Result<u64> {
        if n == 0 {
            return Ok(0);
        }
        match &self.body {
            Body::Leaf(s, at) => Ok((s.select(s.rank(*at)? + n)?.saturating_sub(*at)).min(upto)),
            Body::Branch(l, _) if upto <= l.len => l.start(n, upto),
            Body::Branch(l, r) => {
                let left_count = l.prefix(l.len)? - u64::from(l.last == 13 && r.first == 10);
                if n <= left_count {
                    l.start(n, l.len)
                } else {
                    Ok(l.len + r.start(n - left_count, upto - l.len)?)
                }
            }
        }
    }
    pub fn pieces(
        &self,
        f: &mut impl FnMut(&Arc<Source>, u64, u64) -> io::Result<()>,
    ) -> io::Result<()> {
        match &self.body {
            Body::Leaf(s, at) => f(s, *at, self.len),
            Body::Branch(l, r) => {
                l.pieces(f)?;
                r.pieces(f)
            }
        }
    }
}

pub(super) struct Leaves<'a> {
    nodes: Vec<&'a Node>,
    reverse: bool,
}

impl<'a> Leaves<'a> {
    pub(super) fn new(root: &'a Root, reverse: bool) -> Self {
        Self { nodes: root.as_deref().into_iter().collect(), reverse }
    }

    pub(super) fn next(&mut self) -> Option<(&'a Arc<Source>, u64, u64)> {
        while let Some(node) = self.nodes.pop() {
            match &node.body {
                Body::Leaf(source, at) => return Some((source, *at, node.len)),
                Body::Branch(left, right) => {
                    if self.reverse {
                        self.nodes.push(left);
                        self.nodes.push(right);
                    } else {
                        self.nodes.push(right);
                        self.nodes.push(left);
                    }
                }
            }
        }
        None
    }
}

pub(super) fn shared_bytes(old: &Root, new: &Root, reverse: bool) -> u64 {
    let mut left = Leaves::new(old, reverse);
    let mut right = Leaves::new(new, reverse);
    let mut a = left.next();
    let mut b = right.next();
    let mut shared = 0;
    while let (Some((sa, pa, la)), Some((sb, pb, lb))) = (a, b) {
        let aligned = if reverse { pa + la == pb + lb } else { pa == pb };
        if !aligned || !Arc::ptr_eq(sa, sb) { break; }
        let take = la.min(lb);
        shared += take;
        let skip = if reverse { 0 } else { take };
        a = if la == take { left.next() } else { Some((sa, pa + skip, la - take)) };
        b = if lb == take { right.next() } else { Some((sb, pb + skip, lb - take)) };
    }
    shared
}

