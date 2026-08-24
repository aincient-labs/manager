# Pack CI: prove the committed CSS is the built CSS, then build the image.
# (The full reference pipeline — boot + converge + gallery smoke + kind-check —
# ships with Atelier's client-CI workflow; extend this file from it.)
name: build
on:
  push:
    branches: [main]
  pull_request:
jobs:
  css:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with: { node-version: 22 }
      - name: Build CSS and assert no diff vs the committed file
        run: |
          npx -y @tailwindcss/cli@4 -i build/input.css -o "assets/__MODULE__.css"
          git diff --exit-code -- assets/
  image:
    runs-on: ubuntu-latest
    needs: css
    steps:
      - uses: actions/checkout@v4
      - name: Build the deployable image
        run: docker build .
