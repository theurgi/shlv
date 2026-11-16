-- A one-line description of what belongs under a tag, for the tagging layers to match against
-- (task 027). Mirrored from a declared canonical list, or set with `shlv tag describe`.
ALTER TABLE tag ADD COLUMN description TEXT;
