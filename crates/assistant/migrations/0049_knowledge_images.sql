CREATE TABLE knowledge_images (
    id uuid PRIMARY KEY,
    mime_type text NOT NULL,
    image bytea NOT NULL
);
