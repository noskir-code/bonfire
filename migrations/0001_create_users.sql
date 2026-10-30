-- Users table. The ALTER statements also bring up to date a `users` table
-- created by an earlier version of the code, before migrations were used.

CREATE TABLE IF NOT EXISTS users (
    id SERIAL PRIMARY KEY,
    username TEXT NOT NULL,
    displayname TEXT NOT NULL DEFAULT '',
    firstname TEXT NOT NULL DEFAULT '',
    lastname TEXT NOT NULL DEFAULT '',
    birthday DATE,
    photo_url TEXT,
    background_url TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

ALTER TABLE users ADD COLUMN IF NOT EXISTS displayname TEXT NOT NULL DEFAULT '';
ALTER TABLE users ADD COLUMN IF NOT EXISTS firstname TEXT NOT NULL DEFAULT '';
ALTER TABLE users ADD COLUMN IF NOT EXISTS lastname TEXT NOT NULL DEFAULT '';
ALTER TABLE users ADD COLUMN IF NOT EXISTS birthday DATE;
ALTER TABLE users ADD COLUMN IF NOT EXISTS photo_url TEXT;
ALTER TABLE users ADD COLUMN IF NOT EXISTS background_url TEXT;
ALTER TABLE users ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

-- Birthday is unknown until the user gives it.
ALTER TABLE users ALTER COLUMN birthday DROP NOT NULL;
ALTER TABLE users ALTER COLUMN birthday DROP DEFAULT;

-- Like Mastodon: case is kept, but "Alice" and "alice" can't both exist.
-- The old case-sensitive constraint is replaced by this index.
ALTER TABLE users DROP CONSTRAINT IF EXISTS users_username_key;
CREATE UNIQUE INDEX IF NOT EXISTS users_username_lower_idx ON users (LOWER(username));
