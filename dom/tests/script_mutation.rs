use wild_buzzard_dom::bindings::{
    CreatedNodeToken, DomRootProvider, DomRootTrace, RootedNodeHandle, ScriptMutationBatch,
    ScriptMutationCommand, ScriptMutationError, ScriptMutationLimitKind, ScriptMutationLimits,
    ScriptMutationTokenError, ScriptNode,
};
use wild_buzzard_dom::{Document, DocumentId, DomError, Namespace, NodeId, NodeKind};

fn token(index: u32) -> CreatedNodeToken {
    CreatedNodeToken::from_index(index)
}

fn basic_document() -> (Document, NodeId, NodeId) {
    let mut document = Document::new();
    let html = document.create_html_element("html").unwrap();
    let body = document.create_html_element("body").unwrap();
    document
        .append_child(document.document_node(), html)
        .unwrap();
    document.append_child(html, body).unwrap();
    (document, html, body)
}

fn create_element(index: u32, local_name: &str) -> ScriptMutationCommand {
    ScriptMutationCommand::CreateHtmlElement {
        token: token(index),
        local_name: local_name.into(),
    }
}

fn assert_unchanged(
    document: &Document,
    version: wild_buzzard_dom::DocumentVersion,
    nodes: &[wild_buzzard_dom::SnapshotNode],
) {
    assert_eq!(document.version(), version);
    assert_eq!(
        document.snapshot().unwrap().nodes_in_document_order(),
        nodes
    );
}

#[test]
fn successful_mixed_batch_publishes_one_version_and_deterministic_mapping() {
    let (mut document, html, body) = basic_document();
    let before = document.version();
    let commands = vec![
        create_element(0, "SECTION"),
        ScriptMutationCommand::CreateText {
            token: token(1),
            data: "before".into(),
        },
        create_element(2, "EM"),
        ScriptMutationCommand::AppendChild {
            parent: body.into(),
            child: token(0).into(),
        },
        ScriptMutationCommand::AppendChild {
            parent: token(0).into(),
            child: token(1).into(),
        },
        ScriptMutationCommand::AppendChild {
            parent: token(0).into(),
            child: token(2).into(),
        },
        ScriptMutationCommand::InsertBefore {
            parent: token(0).into(),
            child: token(2).into(),
            reference: Some(token(1).into()),
        },
        ScriptMutationCommand::SetHtmlAttribute {
            element: token(0).into(),
            local_name: "CLASS".into(),
            value: "hero".into(),
        },
        ScriptMutationCommand::SetHtmlAttribute {
            element: token(0).into(),
            local_name: "DATA-X".into(),
            value: "temporary".into(),
        },
        ScriptMutationCommand::SetCharacterData {
            node: token(1).into(),
            data: "after".into(),
        },
        ScriptMutationCommand::RemoveChild {
            parent: token(0).into(),
            child: token(2).into(),
        },
        ScriptMutationCommand::AppendChild {
            parent: body.into(),
            child: token(2).into(),
        },
        ScriptMutationCommand::RemoveHtmlAttribute {
            element: token(0).into(),
            local_name: "DaTa-X".into(),
        },
    ];

    let commit = document
        .apply_script_mutations(
            ScriptMutationBatch::new(before, commands),
            ScriptMutationLimits::DEFAULT,
        )
        .unwrap();
    let section = commit.created_node(token(0)).unwrap();
    let text = commit.created_node(token(1)).unwrap();
    let emphasis = commit.created_node(token(2)).unwrap();

    assert_eq!(commit.created_nodes(), &[section, text, emphasis]);
    assert_eq!(commit.created_node(token(3)), None);
    assert_eq!(commit.version().document_id(), before.document_id());
    assert_eq!(commit.version().revision(), before.revision() + 1);
    assert_eq!(document.version(), commit.version());
    assert_eq!(commit.snapshot().version(), commit.version());
    assert_eq!(document.children(body).unwrap(), &[section, emphasis]);
    assert_eq!(document.children(section).unwrap(), &[text]);
    assert_eq!(document.parent(emphasis).unwrap(), Some(body));
    assert!(matches!(
        document.node_kind(section),
        Ok(NodeKind::Element(data))
            if data.name.namespace == Namespace::Html
                && data.name.local_name == "section"
                && data.html_attribute("class") == Some("hero")
                && data.html_attribute("data-x").is_none()
                && data.attributes[0].name.namespace.is_none()
    ));
    assert!(matches!(
        document.node_kind(text),
        Ok(NodeKind::Text(data)) if data == "after"
    ));
    let order: Vec<_> = commit
        .snapshot()
        .nodes_in_document_order()
        .iter()
        .map(|node| node.id)
        .collect();
    assert_eq!(
        order,
        vec![
            document.document_node(),
            html,
            body,
            section,
            text,
            emphasis
        ]
    );
    document.validate_invariants().unwrap();
}

#[test]
fn stale_and_foreign_versions_fail_before_mutating() {
    let (mut document, _, body) = basic_document();
    let stale = document.version();
    document.set_html_attribute(body, "id", "newer").unwrap();
    let current = document.version();
    let nodes = document
        .snapshot()
        .unwrap()
        .nodes_in_document_order()
        .to_vec();

    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(stale, vec![create_element(0, "p")]),
            ScriptMutationLimits::DEFAULT,
        ),
        Err(ScriptMutationError::VersionMismatch { expected, actual })
            if expected == stale && actual == current
    ));
    assert_unchanged(&document, current, &nodes);

    let foreign = Document::new().version();
    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(foreign, vec![create_element(0, "p")]),
            ScriptMutationLimits::DEFAULT,
        ),
        Err(ScriptMutationError::VersionMismatch { expected, actual })
            if expected == foreign && actual == current
    ));
    assert_unchanged(&document, current, &nodes);
}

#[test]
fn cross_document_node_and_reference_are_indexed_and_atomic() {
    let (mut document, _, body) = basic_document();
    let mut foreign = Document::new();
    let alien = foreign.create_html_element("aside").unwrap();
    let version = document.version();
    let nodes = document
        .snapshot()
        .unwrap()
        .nodes_in_document_order()
        .to_vec();

    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![ScriptMutationCommand::AppendChild {
                    parent: body.into(),
                    child: alien.into(),
                }],
            ),
            ScriptMutationLimits::DEFAULT,
        ),
        Err(ScriptMutationError::Command {
            command_index: 0,
            error: DomError::WrongDocument { .. },
        })
    ));
    assert_unchanged(&document, version, &nodes);

    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![
                    create_element(0, "section"),
                    ScriptMutationCommand::InsertBefore {
                        parent: body.into(),
                        child: token(0).into(),
                        reference: Some(alien.into()),
                    },
                ],
            ),
            ScriptMutationLimits::DEFAULT,
        ),
        Err(ScriptMutationError::Command {
            command_index: 1,
            error: DomError::WrongDocument { .. },
        })
    ));
    assert_unchanged(&document, version, &nodes);
}

#[test]
fn forward_gapped_and_duplicate_tokens_fail_with_command_index() {
    let (mut document, _, body) = basic_document();
    let version = document.version();

    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![
                    ScriptMutationCommand::AppendChild {
                        parent: body.into(),
                        child: token(0).into(),
                    },
                    create_element(0, "p"),
                ],
            ),
            ScriptMutationLimits::DEFAULT,
        ),
        Err(ScriptMutationError::Token {
            command_index: 0,
            error: ScriptMutationTokenError::Unavailable {
                token: unavailable,
                available_created_nodes: 0,
            },
        }) if unavailable == token(0)
    ));
    assert_eq!(document.version(), version);

    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(version, vec![create_element(1, "p")]),
            ScriptMutationLimits::DEFAULT,
        ),
        Err(ScriptMutationError::Token {
            command_index: 0,
            error: ScriptMutationTokenError::CreationOrder { expected, actual },
        }) if expected == token(0) && actual == token(1)
    ));
    assert_eq!(document.version(), version);

    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![create_element(0, "p"), create_element(0, "span")],
            ),
            ScriptMutationLimits::DEFAULT,
        ),
        Err(ScriptMutationError::Token {
            command_index: 1,
            error: ScriptMutationTokenError::CreationOrder { expected, actual },
        }) if expected == token(1) && actual == token(0)
    ));
    assert_eq!(document.version(), version);
}

#[test]
fn hierarchy_cycle_and_reference_failures_roll_back() {
    let (mut document, html, body) = basic_document();
    let version = document.version();
    let nodes = document
        .snapshot()
        .unwrap()
        .nodes_in_document_order()
        .to_vec();

    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![ScriptMutationCommand::AppendChild {
                    parent: body.into(),
                    child: html.into(),
                }],
            ),
            ScriptMutationLimits::DEFAULT,
        ),
        Err(ScriptMutationError::Command {
            command_index: 0,
            error: DomError::Cycle,
        })
    ));
    assert_unchanged(&document, version, &nodes);

    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![
                    ScriptMutationCommand::CreateText {
                        token: token(0),
                        data: "leaf".into(),
                    },
                    ScriptMutationCommand::AppendChild {
                        parent: token(0).into(),
                        child: body.into(),
                    },
                ],
            ),
            ScriptMutationLimits::DEFAULT,
        ),
        Err(ScriptMutationError::Command {
            command_index: 1,
            error: DomError::HierarchyRequest,
        })
    ));
    assert_unchanged(&document, version, &nodes);

    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![
                    create_element(0, "section"),
                    ScriptMutationCommand::InsertBefore {
                        parent: body.into(),
                        child: token(0).into(),
                        reference: Some(html.into()),
                    },
                ],
            ),
            ScriptMutationLimits::DEFAULT,
        ),
        Err(ScriptMutationError::Command {
            command_index: 1,
            error: DomError::ReferenceIsNotChild,
        })
    ));
    assert_unchanged(&document, version, &nodes);
}

#[test]
fn mid_batch_dom_error_does_not_publish_or_consume_an_arena_slot() {
    let (mut document, _, body) = basic_document();
    let version = document.version();
    let nodes = document
        .snapshot()
        .unwrap()
        .nodes_in_document_order()
        .to_vec();

    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![
                    create_element(0, "p"),
                    ScriptMutationCommand::AppendChild {
                        parent: body.into(),
                        child: token(0).into(),
                    },
                    ScriptMutationCommand::SetHtmlAttribute {
                        element: token(0).into(),
                        local_name: "id".into(),
                        value: "rolled-back".into(),
                    },
                    ScriptMutationCommand::SetCharacterData {
                        node: token(0).into(),
                        data: "not character data".into(),
                    },
                ],
            ),
            ScriptMutationLimits::DEFAULT,
        ),
        Err(ScriptMutationError::Command {
            command_index: 3,
            error: DomError::NotCharacterData,
        })
    ));
    assert_unchanged(&document, version, &nodes);

    let commit = document
        .apply_script_mutations(
            ScriptMutationBatch::new(version, vec![create_element(0, "p")]),
            ScriptMutationLimits::DEFAULT,
        )
        .unwrap();
    let created = commit.created_node(token(0)).unwrap();
    assert_eq!(created.slot(), body.slot() + 1);
    assert_eq!(document.version().revision(), version.revision() + 1);
}

#[test]
fn hard_caps_and_every_configured_limit_fail_closed() {
    let too_many_commands =
        ScriptMutationLimits::try_new(ScriptMutationLimits::HARD_MAX_COMMANDS + 1, 0, 0, 0)
            .unwrap_err();
    assert_eq!(too_many_commands.kind, ScriptMutationLimitKind::Commands);
    assert_eq!(
        ScriptMutationLimits::try_new(0, ScriptMutationLimits::HARD_MAX_CREATED_NODES + 1, 0, 0,)
            .unwrap_err()
            .kind,
        ScriptMutationLimitKind::CreatedNodes
    );
    assert_eq!(
        ScriptMutationLimits::try_new(0, 0, ScriptMutationLimits::HARD_MAX_STRING_BYTES + 1, 0,)
            .unwrap_err()
            .kind,
        ScriptMutationLimitKind::StringBytes
    );
    assert_eq!(
        ScriptMutationLimits::try_new(
            0,
            0,
            0,
            ScriptMutationLimits::HARD_MAX_TOTAL_STRING_BYTES + 1,
        )
        .unwrap_err()
        .kind,
        ScriptMutationLimitKind::TotalStringBytes
    );

    let (mut document, _, _) = basic_document();
    let version = document.version();
    let command_limit = ScriptMutationLimits::try_new(1, 2, 8, 16).unwrap();
    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![create_element(0, "p"), create_element(1, "i")],
            ),
            command_limit,
        ),
        Err(ScriptMutationError::LimitExceeded {
            command_index: 1,
            kind: ScriptMutationLimitKind::Commands,
            limit: 1,
            actual: 2,
        })
    ));

    let create_limit = ScriptMutationLimits::try_new(2, 1, 8, 16).unwrap();
    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![create_element(0, "p"), create_element(1, "i")],
            ),
            create_limit,
        ),
        Err(ScriptMutationError::LimitExceeded {
            command_index: 1,
            kind: ScriptMutationLimitKind::CreatedNodes,
            limit: 1,
            actual: 2,
        })
    ));

    let string_limit = ScriptMutationLimits::try_new(1, 1, 2, 8).unwrap();
    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(version, vec![create_element(0, "div")]),
            string_limit,
        ),
        Err(ScriptMutationError::LimitExceeded {
            command_index: 0,
            kind: ScriptMutationLimitKind::StringBytes,
            limit: 2,
            actual: 3,
        })
    ));

    let total_limit = ScriptMutationLimits::try_new(2, 2, 3, 5).unwrap();
    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![
                    create_element(0, "div"),
                    ScriptMutationCommand::CreateText {
                        token: token(1),
                        data: "abc".into(),
                    },
                ],
            ),
            total_limit,
        ),
        Err(ScriptMutationError::LimitExceeded {
            command_index: 1,
            kind: ScriptMutationLimitKind::TotalStringBytes,
            limit: 5,
            actual: 6,
        })
    ));
    assert_eq!(document.version(), version);
}

#[test]
fn names_values_text_and_remove_names_all_count_toward_string_budgets() {
    let (mut document, _, body) = basic_document();
    let version = document.version();
    let limits = ScriptMutationLimits::try_new(1, 0, 4, 5).unwrap();

    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![ScriptMutationCommand::SetHtmlAttribute {
                    element: body.into(),
                    local_name: "id".into(),
                    value: "four".into(),
                }],
            ),
            limits,
        ),
        Err(ScriptMutationError::LimitExceeded {
            command_index: 0,
            kind: ScriptMutationLimitKind::TotalStringBytes,
            limit: 5,
            actual: 6,
        })
    ));

    let zero_strings = ScriptMutationLimits::try_new(1, 0, 0, 0).unwrap();
    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![ScriptMutationCommand::RemoveHtmlAttribute {
                    element: body.into(),
                    local_name: "x".into(),
                }],
            ),
            zero_strings,
        ),
        Err(ScriptMutationError::LimitExceeded {
            command_index: 0,
            kind: ScriptMutationLimitKind::StringBytes,
            limit: 0,
            actual: 1,
        })
    ));

    let text_limits = ScriptMutationLimits::try_new(1, 1, 2, 2).unwrap();
    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![ScriptMutationCommand::CreateText {
                    token: token(0),
                    data: "abc".into(),
                }],
            ),
            text_limits,
        ),
        Err(ScriptMutationError::LimitExceeded {
            command_index: 0,
            kind: ScriptMutationLimitKind::StringBytes,
            limit: 2,
            actual: 3,
        })
    ));
    assert_eq!(document.version(), version);
}

#[test]
fn zero_limits_and_empty_batches_have_deterministic_behavior() {
    let (mut document, _, body) = basic_document();
    let child = document.create_html_element("p").unwrap();
    document.append_child(body, child).unwrap();
    let version = document.version();
    let zero = ScriptMutationLimits::try_new(0, 0, 0, 0).unwrap();

    assert!(matches!(
        document.apply_script_mutations(ScriptMutationBatch::new(version, Vec::new()), zero,),
        Err(ScriptMutationError::EmptyBatch)
    ));
    assert!(matches!(
        document.apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![ScriptMutationCommand::RemoveChild {
                    parent: body.into(),
                    child: child.into(),
                }],
            ),
            zero,
        ),
        Err(ScriptMutationError::LimitExceeded {
            command_index: 0,
            kind: ScriptMutationLimitKind::Commands,
            limit: 0,
            actual: 1,
        })
    ));

    let no_strings = ScriptMutationLimits::try_new(1, 0, 0, 0).unwrap();
    let commit = document
        .apply_script_mutations(
            ScriptMutationBatch::new(
                version,
                vec![ScriptMutationCommand::RemoveChild {
                    parent: body.into(),
                    child: child.into(),
                }],
            ),
            no_strings,
        )
        .unwrap();
    assert_eq!(commit.version().revision(), version.revision() + 1);
    assert_eq!(document.parent(child).unwrap(), None);
}

#[test]
fn removing_an_absent_html_attribute_still_commits_one_batch_version() {
    let (mut document, _, body) = basic_document();
    let before = document.snapshot().unwrap();
    assert_eq!(document.attribute(body, None, "missing").unwrap(), None);

    let commit = document
        .apply_script_mutations(
            ScriptMutationBatch::new(
                before.version(),
                vec![ScriptMutationCommand::RemoveHtmlAttribute {
                    element: body.into(),
                    local_name: "MISSING".into(),
                }],
            ),
            ScriptMutationLimits::DEFAULT,
        )
        .unwrap();

    assert_eq!(commit.version().document_id(), before.document_id());
    assert_eq!(commit.version().revision(), before.revision() + 1);
    assert_eq!(document.version(), commit.version());
    assert_eq!(document.attribute(body, None, "missing").unwrap(), None);
    assert_eq!(
        commit.snapshot().nodes_in_document_order(),
        before.nodes_in_document_order()
    );
}

#[derive(Clone)]
struct TestRoot {
    document: DocumentId,
    node: NodeId,
}

impl RootedNodeHandle for TestRoot {
    fn document_id(&self) -> DocumentId {
        self.document
    }

    fn node_id(&self) -> NodeId {
        self.node
    }
}

struct TestRootProvider {
    document: DocumentId,
}

impl DomRootProvider for TestRootProvider {
    type Root = TestRoot;
    type Error = ();

    fn root_node(&self, node: NodeId) -> Result<Self::Root, Self::Error> {
        if node.document_id() != self.document {
            return Err(());
        }
        Ok(TestRoot {
            document: self.document,
            node,
        })
    }

    fn is_live(&self, root: &Self::Root) -> bool {
        root.document_id() == self.document
    }
}

struct RootHolder(TestRoot);

impl DomRootTrace for RootHolder {
    fn trace_dom_roots(&self, visitor: &mut dyn FnMut(NodeId)) {
        visitor(self.0.node_id());
    }
}

#[test]
fn mutation_operands_do_not_replace_the_existing_rooting_contract() {
    let (document, _, body) = basic_document();
    let provider = TestRootProvider {
        document: document.id(),
    };
    let root = provider.root_node(body).unwrap();
    assert!(provider.is_live(&root));
    let holder = RootHolder(root);
    let mut visited = Vec::new();
    holder.trace_dom_roots(&mut |node| visited.push(node));
    assert_eq!(visited, vec![body]);

    let operand = ScriptNode::Existing(body);
    assert_eq!(operand, ScriptNode::Existing(holder.0.node_id()));
}
