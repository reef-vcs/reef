-- R3.40: paired devices (ADR 0095). A device's token is a row of scope 'read' with the address it was
-- paired at, `[remote] origin` then. Code that does not know the column reads it as a read token,
-- and that is the safer reading; a 'write' row never has one. Released migrations are never edited.
ALTER TABLE remote_tokens ADD COLUMN device_origin TEXT;
