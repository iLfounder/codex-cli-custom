use super::*;

fn rendered(cell: &impl HistoryCell) -> String {
    cell.display_lines(/*width*/ 72)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn presentation_card_notice_and_progress_snapshot() {
    let cells = [
        ThreadPresentation::Card {
            id: "summary".to_string(),
            title: "Release summary".to_string(),
            body: "Three patches are ready for review.".to_string(),
        },
        ThreadPresentation::Notice {
            id: "warning".to_string(),
            level: ThreadPresentationNoticeLevel::Warning,
            message: "Approval is still required.".to_string(),
        },
        ThreadPresentation::Progress {
            id: "build".to_string(),
            label: "Building artifacts".to_string(),
            current: 7,
            total: Some(10),
        },
    ]
    .map(|item| rendered(&ThreadPresentationHistoryCell::new(item)))
    .join("\n\n");

    insta::assert_snapshot!(cells, @r#"
    ◆ Release summary
      Three patches are ready for review.

    ▲ Approval is still required.

    ◒ Building artifacts  7/10
    "#);
}
