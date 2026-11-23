# The factory's systemd user units

Copies of the units DotFiles keeps in its `factory` stow package
(`factory/.config/systemd/user/`), which `factory install --units <dir>` links into
`~/.config/systemd/user`. The install tests link these and run the daemon the way they say.
Change both copies together.
