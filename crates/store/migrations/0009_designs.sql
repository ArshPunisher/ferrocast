-- Designs (API.md §4 Designs): the streamer's own alert looks, themes, scenes and widget
-- styles, each the whole `Design` as JSON. Built-in designs ship inside the app and are
-- never stored here, so an update can bring new ones without a migration.
CREATE TABLE designs (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL CHECK (kind IN ('alert', 'theme', 'scene', 'widget')),
    name        TEXT NOT NULL,
    data        TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX designs_by_kind ON designs(kind, updated_at);

-- The gallery design an overlay was made from: `{ id, scene, branded }` as JSON, or NULL
-- for overlays built by hand. A built-in keeps its small Ferrocast mark through this.
ALTER TABLE overlays ADD COLUMN design TEXT;
