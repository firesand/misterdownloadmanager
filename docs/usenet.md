# Usenet (NZB files)

Mister Download Manager downloads the files an NZB file describes from your Usenet provider's
news server.

1. **Tasks → Usenet server…**: the server's address and port, **Secure connection (TLS)** (port
   563; without it, 119), your user name and password, and how many connections your account
   allows at once. **Test** connects and signs in, and says what the server answered. The
   password is kept like the proxy's: in the system's keychain where there is one.
2. **Tasks → Open NZB file…**, or drop an `.nzb` on the window, or give it on the command line
   (`misterdownloadmanager file.nzb`).

The download is one row in the list, marked **Usenet**. An NZB with one file is saved into that
file type's category folder; one with several gets a folder of its own, named after the NZB.
Pause and resume work as for any download (finished articles aren't fetched again), and the
Speed Limiter applies.

An article the server no longer has leaves its file incomplete: the other files are finished,
the row says how many articles are missing, and the unfinished file stays as a `.mdm.part`
file. Par2 repair and unpacking are not done for you (Link grabber's archive extraction works
on the finished files as on any others).

Where it comes from: [Plexo](https://github.com/anmolkapil/plexo)'s Usenet downloads; the code
is Mister Download Manager's own (`crates/usenet`). For trying it without an account,
`cargo run -p mdm-usenet --example newsserver -- lab.nzb` runs a news server on this computer
(`lab` / `lab`, no TLS) and writes an NZB for it.
