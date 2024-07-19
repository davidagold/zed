use gpui::actions;

actions!(
    notebook,
    [
        RunCurrentCell,
        RunCurrentSelection,
        InsertCellAbove,
        InsertCellBelow
    ]
);
