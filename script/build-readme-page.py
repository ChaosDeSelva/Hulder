#!/usr/bin/env python3
"""Render the dark README page. Install the renderer with: python3 -m pip install Markdown"""
from pathlib import Path
import re
import markdown

root = Path(__file__).resolve().parent.parent
source = (root / 'README.md').read_text()
body = markdown.markdown(source, extensions=['tables', 'fenced_code', 'toc'])
body = body.replace('src="assets/hulder-banner.png"', 'src="assets/banner.png"')
# Documentation outside the static site remains readable on GitHub.
body = re.sub(r'href="((?!https?://|#)[^"\s]+\.md(?:#[^"\s]*)?)"',
              r'href="https://github.com/ChaosDeSelva/Hulder/blob/main/\1"', body)
page = '''<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="theme-color" content="#101914">
<title>Hulder · Project guide</title>
<link rel="icon" href="assets/icon.svg" type="image/svg+xml">
<link rel="stylesheet" href="guide.css"></head>
<body><a class="skip" href="#guide">Skip to guide</a>
<nav aria-label="Guide navigation"><a href="./">Hulder</a><a href="https://github.com/ChaosDeSelva/Hulder">View source on GitHub</a></nav>
<main id="guide">''' + body + '</main></body></html>\n'
(root / 'site/guide.html').write_text(page)
(root / 'site/README.txt').write_text(source)
