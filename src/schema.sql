CREATE TABLE IF NOT EXISTS usage(bytes INTEGER NOT NULL);
INSERT INTO usage SELECT 0 WHERE NOT EXISTS (SELECT 1 FROM usage);
CREATE TABLE IF NOT EXISTS counters(name TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS entries(
    id INTEGER PRIMARY KEY, key BLOB UNIQUE NOT NULL, recipe BLOB NOT NULL,
    access INTEGER NOT NULL, stderr BLOB NOT NULL, stderr_hash BLOB NOT NULL, compile_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS recipes ON entries(recipe,access DESC);
CREATE INDEX IF NOT EXISTS lru ON entries(access);
CREATE TABLE IF NOT EXISTS files(
    entry INTEGER REFERENCES entries ON DELETE CASCADE, slot INTEGER, digest BLOB NOT NULL,
    size INTEGER NOT NULL, mode INTEGER NOT NULL, single INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(entry,slot)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS artifacts ON files(digest);
CREATE INDEX IF NOT EXISTS artifact_sizes ON files(size);
-- Keep the legacy refs column for existing databases. The files and parts
-- indexes identify live users without dirtying a compressed payload.
CREATE TABLE IF NOT EXISTS chunks(
    hash BLOB PRIMARY KEY, raw INTEGER NOT NULL, data BLOB NOT NULL, refs INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS parts(
    entry INTEGER REFERENCES entries ON DELETE CASCADE, slot INTEGER, ordinal INTEGER, chunk BLOB NOT NULL,
    PRIMARY KEY(entry,slot,ordinal)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS chunk_users ON parts(chunk);
CREATE TRIGGER IF NOT EXISTS add_entry AFTER INSERT ON entries BEGIN
    UPDATE usage SET bytes=bytes+320+length(NEW.stderr);
END;
CREATE TRIGGER IF NOT EXISTS drop_entry AFTER DELETE ON entries BEGIN
    UPDATE usage SET bytes=bytes-320-length(OLD.stderr);
END;
CREATE TRIGGER IF NOT EXISTS add_file AFTER INSERT ON files BEGIN
    UPDATE usage SET bytes=bytes+128;
END;
CREATE TRIGGER IF NOT EXISTS drop_file AFTER DELETE ON files BEGIN
    UPDATE usage SET bytes=bytes-128;
END;
CREATE TRIGGER IF NOT EXISTS drop_single AFTER DELETE ON files WHEN OLD.single=1 BEGIN
    DELETE FROM chunks WHERE hash=OLD.digest
        AND NOT EXISTS(SELECT 1 FROM files WHERE digest=OLD.digest AND single=1)
        AND NOT EXISTS(SELECT 1 FROM parts WHERE chunk=OLD.digest);
END;
CREATE TRIGGER IF NOT EXISTS add_chunk AFTER INSERT ON chunks BEGIN
    UPDATE usage SET bytes=bytes+96+length(NEW.data);
END;
CREATE TRIGGER IF NOT EXISTS drop_chunk AFTER DELETE ON chunks BEGIN
    UPDATE usage SET bytes=bytes-96-length(OLD.data);
END;
CREATE TRIGGER IF NOT EXISTS add_part AFTER INSERT ON parts BEGIN
    UPDATE usage SET bytes=bytes+96;
END;
CREATE TRIGGER IF NOT EXISTS drop_part AFTER DELETE ON parts BEGIN
    UPDATE usage SET bytes=bytes-96;
    DELETE FROM chunks WHERE hash=OLD.chunk
        AND NOT EXISTS(SELECT 1 FROM files WHERE digest=OLD.chunk AND single=1)
        AND NOT EXISTS(SELECT 1 FROM parts WHERE chunk=OLD.chunk);
END;
CREATE TABLE IF NOT EXISTS manifests(
    id INTEGER PRIMARY KEY, hash BLOB NOT NULL UNIQUE, data BLOB NOT NULL, refs INTEGER NOT NULL
);
CREATE TRIGGER IF NOT EXISTS add_manifest AFTER INSERT ON manifests BEGIN
    UPDATE usage SET bytes=bytes+96+length(NEW.data);
END;
CREATE TRIGGER IF NOT EXISTS drop_manifest AFTER DELETE ON manifests BEGIN
    UPDATE usage SET bytes=bytes-96-length(OLD.data);
END;
CREATE TABLE IF NOT EXISTS plans(
    entry INTEGER PRIMARY KEY REFERENCES entries ON DELETE CASCADE, inputs BLOB NOT NULL, topology BLOB NOT NULL, codegen INTEGER NOT NULL DEFAULT 0, manifest INTEGER REFERENCES manifests
);
CREATE INDEX IF NOT EXISTS manifest_users ON plans(manifest) WHERE manifest IS NOT NULL;
CREATE TRIGGER IF NOT EXISTS release_manifest AFTER DELETE ON plans WHEN OLD.manifest IS NOT NULL BEGIN
    UPDATE manifests SET refs=refs-1 WHERE id=OLD.manifest;
    DELETE FROM manifests WHERE id=OLD.manifest AND refs=0;
END;
CREATE TRIGGER IF NOT EXISTS add_plan AFTER INSERT ON plans BEGIN
    UPDATE usage SET bytes=bytes+80+length(NEW.inputs);
END;
CREATE TRIGGER IF NOT EXISTS drop_plan AFTER DELETE ON plans BEGIN
    UPDATE usage SET bytes=bytes-80-length(OLD.inputs);
END;
CREATE TABLE IF NOT EXISTS compilers(identity BLOB PRIMARY KEY, stable INTEGER NOT NULL, seen INTEGER NOT NULL) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS bound_compilers AFTER INSERT ON compilers BEGIN
    DELETE FROM compilers WHERE identity IN (SELECT identity FROM compilers ORDER BY seen DESC LIMIT -1 OFFSET 32);
END;
CREATE TABLE IF NOT EXISTS needs(
    entry INTEGER REFERENCES entries ON DELETE CASCADE, digest BLOB NOT NULL, codegen INTEGER NOT NULL,
    PRIMARY KEY(entry,digest)
) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS add_need AFTER INSERT ON needs BEGIN UPDATE usage SET bytes=bytes+96; END;
CREATE TRIGGER IF NOT EXISTS drop_need AFTER DELETE ON needs BEGIN UPDATE usage SET bytes=bytes-96; END;
CREATE TABLE IF NOT EXISTS objects(hash BLOB PRIMARY KEY, verity BLOB NOT NULL, bytes INTEGER NOT NULL, refs INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS clones(entry INTEGER REFERENCES entries ON DELETE CASCADE, slot INTEGER, hash BLOB NOT NULL, PRIMARY KEY(entry,slot)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS clone_users ON clones(hash);
CREATE TABLE IF NOT EXISTS retired(hash BLOB PRIMARY KEY) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS add_object AFTER INSERT ON objects BEGIN
    UPDATE usage SET bytes=bytes+NEW.bytes+160;
    DELETE FROM retired WHERE hash=NEW.hash;
END;
CREATE TRIGGER IF NOT EXISTS drop_object AFTER DELETE ON objects BEGIN
    UPDATE usage SET bytes=bytes-OLD.bytes-160;
    INSERT OR IGNORE INTO retired VALUES(OLD.hash);
END;
CREATE TRIGGER IF NOT EXISTS add_clone AFTER INSERT ON clones BEGIN UPDATE usage SET bytes=bytes+96; END;
CREATE TRIGGER IF NOT EXISTS drop_clone AFTER DELETE ON clones BEGIN
    UPDATE usage SET bytes=bytes-96;
    UPDATE objects SET refs=refs-1 WHERE hash=OLD.hash;
    DELETE FROM objects WHERE hash=OLD.hash AND refs=0;
END;
CREATE TABLE IF NOT EXISTS probes(key BLOB PRIMARY KEY,stdout BLOB NOT NULL,stderr BLOB NOT NULL,checksum BLOB NOT NULL,seen INTEGER NOT NULL) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS add_probe AFTER INSERT ON probes BEGIN UPDATE usage SET bytes=bytes+160+length(NEW.stdout)+length(NEW.stderr); END;
CREATE TRIGGER IF NOT EXISTS drop_probe AFTER DELETE ON probes BEGIN UPDATE usage SET bytes=bytes-160-length(OLD.stdout)-length(OLD.stderr); END;
CREATE TABLE IF NOT EXISTS source_variants(entry INTEGER PRIMARY KEY REFERENCES entries ON DELETE CASCADE,digest BLOB NOT NULL);
CREATE INDEX IF NOT EXISTS source_keys ON source_variants(digest);
CREATE TRIGGER IF NOT EXISTS add_variant AFTER INSERT ON source_variants BEGIN UPDATE usage SET bytes=bytes+96; END;
CREATE TRIGGER IF NOT EXISTS drop_variant AFTER DELETE ON source_variants BEGIN UPDATE usage SET bytes=bytes-96; END;
-- Only entries resolved in multiple directory layouts need this row.
CREATE TABLE IF NOT EXISTS worktree_topologies(
    entry INTEGER PRIMARY KEY REFERENCES entries ON DELETE CASCADE,
    latest BLOB NOT NULL, previous BLOB, oldest BLOB
);
CREATE TRIGGER IF NOT EXISTS add_worktree_topology AFTER INSERT ON worktree_topologies BEGIN
    UPDATE usage SET bytes=bytes+192;
END;
CREATE TRIGGER IF NOT EXISTS drop_worktree_topology AFTER DELETE ON worktree_topologies BEGIN
    UPDATE usage SET bytes=bytes-192;
END;
CREATE TABLE IF NOT EXISTS maintenance(name TEXT PRIMARY KEY,last_run INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS input_samples(entry INTEGER PRIMARY KEY REFERENCES entries ON DELETE CASCADE, digest BLOB NOT NULL);
CREATE INDEX IF NOT EXISTS sample_keys ON input_samples(digest);
CREATE TRIGGER IF NOT EXISTS add_input_sample AFTER INSERT ON input_samples BEGIN UPDATE usage SET bytes=bytes+128; END;
CREATE TRIGGER IF NOT EXISTS drop_input_sample AFTER DELETE ON input_samples BEGIN UPDATE usage SET bytes=bytes-128; END;
PRAGMA user_version=13;
