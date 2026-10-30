-- Login credentials. Nullable so rows created before accounts existed stay valid,
-- those users simply can't sign in.

ALTER TABLE users ADD COLUMN IF NOT EXISTS email TEXT;
ALTER TABLE users ADD COLUMN IF NOT EXISTS password_hash TEXT;

-- Same rule as usernames: "Alice@Example.com" and "alice@example.com" are the same email.
CREATE UNIQUE INDEX IF NOT EXISTS users_email_lower_idx ON users (LOWER(email));
