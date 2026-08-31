//! The layout tree that travels with a transfer.

use crate::buf::{Reader, Writer};
use crate::error::{Result, WireError};

/// The tag a tree-shaped `BadValue` is reported against: the Tree message.
const TREE_TAG: u64 = 0x04;

/// One node of the transferred layout.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf { pane_uid: u64 },
    HSplit { ratio: f32, children: Vec<Node> },
    VSplit { ratio: f32, children: Vec<Node> },
    Tabs { active: u32, children: Vec<(String, Node)> },
}

impl Node {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.write(&mut w);
        w.into_vec()
    }

    fn write(&self, w: &mut Writer) {
        match self {
            Node::Leaf { pane_uid } => {
                w.varint(0);
                w.varint(*pane_uid);
            }
            Node::HSplit { ratio, children } | Node::VSplit { ratio, children } => {
                w.varint(if matches!(self, Node::HSplit { .. }) { 1 } else { 2 });
                w.u32le(ratio.to_bits());
                w.varint(children.len() as u64);
                for c in children {
                    c.write(w);
                }
            }
            Node::Tabs { active, children } => {
                w.varint(3);
                w.varint(*active as u64);
                w.varint(children.len() as u64);
                for (title, c) in children {
                    w.str(title);
                    c.write(w);
                }
            }
        }
    }

    pub fn decode(body: &[u8]) -> Result<Node> {
        let mut r = Reader::new(body);
        let n = Node::read(&mut r)?;
        r.finish()?;
        Ok(n)
    }

    fn read(r: &mut Reader<'_>) -> Result<Node> {
        let kind = r.varint()?;
        match kind {
            0 => Ok(Node::Leaf { pane_uid: r.varint()? }),
            1 | 2 => {
                let ratio = f32::from_bits(r.u32le()?);
                if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) {
                    return Err(WireError::BadValue { tag: TREE_TAG, why: "split ratio must be finite and within 0.0..=1.0" });
                }
                let n = r.varint()? as usize;
                let mut children = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    children.push(Node::read(r)?);
                }
                Ok(if kind == 1 { Node::HSplit { ratio, children } } else { Node::VSplit { ratio, children } })
            }
            3 => {
                let active = r.varint()? as u32;
                let n = r.varint()? as usize;
                let mut children = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    let title = r.str()?;
                    children.push((title, Node::read(r)?));
                }
                Ok(Node::Tabs { active, children })
            }
            _ => Err(WireError::BadValue { tag: TREE_TAG, why: "node kind must be 0, 1, 2 or 3" }),
        }
    }

    /// Every pane referenced by this tree, depth-first, left to right. The
    /// receiver uses it to check that every leaf got a matching `PaneState`.
    pub fn pane_uids(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.collect_uids(&mut out);
        out
    }

    fn collect_uids(&self, out: &mut Vec<u64>) {
        match self {
            Node::Leaf { pane_uid } => out.push(*pane_uid),
            Node::HSplit { children, .. } | Node::VSplit { children, .. } => {
                for c in children {
                    c.collect_uids(out);
                }
            }
            Node::Tabs { children, .. } => {
                for (_, c) in children {
                    c.collect_uids(out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WireError;

    fn round_trip(n: &Node) -> Node {
        Node::decode(&n.encode()).unwrap()
    }

    #[test]
    fn a_single_pane_is_a_leaf() {
        let n = Node::Leaf { pane_uid: 7 };
        assert_eq!(round_trip(&n), n);
        assert_eq!(n.pane_uids(), vec![7]);
    }

    #[test]
    fn splits_round_trip_with_exact_ratios() {
        let n = Node::HSplit {
            ratio: 0.382,
            children: vec![Node::Leaf { pane_uid: 1 }, Node::VSplit { ratio: 0.5, children: vec![Node::Leaf { pane_uid: 2 }, Node::Leaf { pane_uid: 3 }] }],
        };
        let back = round_trip(&n);
        assert_eq!(back, n);
        match back {
            Node::HSplit { ratio, .. } => assert_eq!(ratio.to_bits(), 0.382f32.to_bits(), "ratio must be bit-exact"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tabs_carry_titles_and_the_active_index() {
        let n = Node::Tabs {
            active: 1,
            children: vec![
                ("build".to_string(), Node::Leaf { pane_uid: 10 }),
                ("claude".to_string(), Node::HSplit { ratio: 0.5, children: vec![Node::Leaf { pane_uid: 11 }, Node::Leaf { pane_uid: 12 }] }),
            ],
        };
        assert_eq!(round_trip(&n), n);
    }

    #[test]
    fn pane_uids_walks_the_whole_forest_in_order() {
        let n = Node::Tabs {
            active: 0,
            children: vec![
                ("a".into(), Node::Leaf { pane_uid: 1 }),
                ("b".into(), Node::VSplit { ratio: 0.5, children: vec![Node::Leaf { pane_uid: 2 }, Node::Leaf { pane_uid: 3 }] }),
            ],
        };
        assert_eq!(n.pane_uids(), vec![1, 2, 3]);
    }

    #[test]
    fn an_unknown_node_kind_is_rejected() {
        let mut w = crate::buf::Writer::new();
        w.varint(9); // there is no kind 9
        assert_eq!(
            Node::decode(&w.into_vec()).unwrap_err(),
            WireError::BadValue { tag: 0x04, why: "node kind must be 0, 1, 2 or 3" }
        );
    }

    #[test]
    fn a_nan_ratio_is_rejected() {
        let mut w = crate::buf::Writer::new();
        w.varint(1); // HSplit
        w.u32le(f32::NAN.to_bits());
        w.varint(0); // no children
        assert_eq!(
            Node::decode(&w.into_vec()).unwrap_err(),
            WireError::BadValue { tag: 0x04, why: "split ratio must be finite and within 0.0..=1.0" }
        );
    }

    #[test]
    fn an_out_of_range_ratio_is_rejected() {
        let mut w = crate::buf::Writer::new();
        w.varint(2); // VSplit
        w.u32le(1.5f32.to_bits());
        w.varint(0);
        assert_eq!(
            Node::decode(&w.into_vec()).unwrap_err(),
            WireError::BadValue { tag: 0x04, why: "split ratio must be finite and within 0.0..=1.0" }
        );
    }

    #[test]
    fn a_deep_tree_round_trips() {
        let mut n = Node::Leaf { pane_uid: 0 };
        for i in 1..64 {
            n = Node::HSplit { ratio: 0.5, children: vec![n, Node::Leaf { pane_uid: i }] };
        }
        assert_eq!(round_trip(&n), n);
    }

    #[test]
    fn trailing_bytes_after_a_tree_are_rejected() {
        let mut bytes = Node::Leaf { pane_uid: 1 }.encode();
        bytes.push(0);
        assert_eq!(Node::decode(&bytes).unwrap_err(), WireError::TrailingBytes { left: 1 });
    }
}
