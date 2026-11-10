# mdm-usenet

Downloads the files an NZB describes from a Usenet (NNTP) server: NZB parsing, a small NNTP
client (implicit TLS, `AUTHINFO`, `BODY`), a yEnc decoder, and a `Download` that runs several
connections (over several network adapters if asked), writes each article at its place in a
`.mdm.part` file, renames finished files, and resumes from `<nzb name>.mdm.nzbstate`.

Credit: the behaviour (NZB subjects and file names, yEnc handling, connection limits, missing
articles) follows the Usenet module of Plexo (MIT licence), which was the reference for this
crate. No Plexo code is copied; the logic is rewritten in Rust.
