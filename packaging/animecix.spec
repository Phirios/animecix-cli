Name:           animecix
Version:        0.1.0
Release:        1
Summary:        Watch and download Turkish-subtitled anime from Animecix
License:        MIT
URL:            https://github.com/Phirios/animecix-cli
Source0:        animecix
Source1:        README.md
Source2:        LICENSE
Source3:        animecix.bash
Source4:        _animecix
Source5:        animecix.fish
Recommends:     mpv
Suggests:       ffmpeg-free

# The release binary is already built; do not generate a misleading debuginfo RPM.
%global debug_package %{nil}

%description
Search Animecix for Turkish-subtitled anime, select an episode and provider,
then watch in a local video player or download it. HLS downloads need FFmpeg.

%prep
%build

%install
install -Dm755 %{SOURCE0} %{buildroot}%{_bindir}/animecix
install -Dm644 %{SOURCE1} %{buildroot}%{_docdir}/%{name}/README.md
install -Dm644 %{SOURCE2} %{buildroot}%{_datadir}/licenses/%{name}/LICENSE
install -Dm644 %{SOURCE3} %{buildroot}%{_datadir}/bash-completion/completions/animecix
install -Dm644 %{SOURCE4} %{buildroot}%{_datadir}/zsh/site-functions/_animecix
install -Dm644 %{SOURCE5} %{buildroot}%{_datadir}/fish/vendor_completions.d/animecix.fish

%files
%{_bindir}/animecix
%doc %{_docdir}/%{name}/README.md
%license %{_datadir}/licenses/%{name}/LICENSE
%{_datadir}/bash-completion/completions/animecix
%{_datadir}/zsh/site-functions/_animecix
%{_datadir}/fish/vendor_completions.d/animecix.fish

%changelog
* Sat Oct 03 2026 Phirios <phirios@users.noreply.github.com> - 0.1.0-1
- Initial Turkish-subtitled anime CLI package.
