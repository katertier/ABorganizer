-- slice 3D.1: language-normalize.
--
-- Adds `books.description_lang` so the UI can render the
-- description with the correct directionality / font even when
-- it differs from `books.language` (a German user with a
-- Japanese book; an English UI displaying a description in
-- the book's original language).
--
-- `description_lang` is stored in the canonical BCP-47
-- primary-subtag form normalize() produces — same shape as
-- `books.language`. NULL = unknown; the UI falls back to
-- LTR Latin-script rendering.

ALTER TABLE books ADD COLUMN description_lang TEXT;
