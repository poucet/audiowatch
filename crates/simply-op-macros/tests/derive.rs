//! `#[derive(Op)]` end-to-end: classification, label templates, defaults.

use simply_history::OpMeta;
use simply_op_macros::Op;

#[derive(Op)]
#[allow(dead_code)] // fields exist to be interpolated by labels
enum Request {
    /// No attribute: a read with the snake_case default label.
    GetGraph {},

    #[op(read)]
    ListModules {},

    #[op(mutates, label = "connect {from_node}.{from_port} -> {to_node}.{to_port}")]
    Connect { from_node: String, from_port: String, to_node: String, to_port: String },

    /// Mutating, but labeled by default (fields unreferenced).
    #[op(mutates)]
    SetParam { node: String, value: f32 },

    /// Templates may skip fields and use format specs.
    #[op(mutates, label = "set {node} = {value:.2}")]
    Tune { node: String, value: f32 },

    #[op(mutates)]
    Wipe,
}

#[derive(Op)]
#[op(mutates, label = "nudge {node}")]
struct Nudge {
    node: String,
}

#[test]
fn classification_defaults_to_read() {
    assert!(!Request::GetGraph {}.mutates());
    assert!(!Request::ListModules {}.mutates());
    assert!(Request::SetParam { node: "1:0".into(), value: 0.5 }.mutates());
    assert!(Request::Wipe.mutates());
    assert!(Nudge { node: "2:0".into() }.mutates());
}

#[test]
fn label_templates_interpolate_fields() {
    let connect = Request::Connect {
        from_node: "1:0".into(),
        from_port: "out".into(),
        to_node: "2:0".into(),
        to_port: "in".into(),
    };
    assert_eq!(connect.label(), "connect 1:0.out -> 2:0.in");
    assert_eq!(
        Request::Tune { node: "osc".into(), value: 0.456 }.label(),
        "set osc = 0.46",
        "format specs pass through"
    );
    assert_eq!(Nudge { node: "3:0".into() }.label(), "nudge 3:0");
}

#[test]
fn default_labels_are_the_snake_case_name() {
    assert_eq!(Request::GetGraph {}.label(), "get_graph");
    assert_eq!(Request::SetParam { node: "x".into(), value: 1.0 }.label(), "set_param");
    assert_eq!(Request::Wipe.label(), "wipe");
}
