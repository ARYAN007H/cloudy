# CloudDrive Setup Guide

## Prerequisites
- CloudDrive installed (see README.md for build instructions)
- 1-4 Google/Gmail accounts you want to use
- A web browser

## Step 1: Create a Google Cloud Project
1. Go to https://console.cloud.google.com/
2. Sign in with one of your Google accounts (it doesn't matter which one)
3. Click the project dropdown at the top of the page (it may say "Select a project" or show a project name)
4. Click "New Project" in the dialog that appears
5. Enter project name: "CloudDrive" (or anything you like)
6. Leave Organization as "No organization"
7. Click "Create"
8. Wait a moment, then make sure the new project is selected in the project dropdown

## Step 2: Enable the Google Drive API
1. In the left sidebar, navigate to "APIs & Services" → "Library" (or go directly to https://console.cloud.google.com/apis/library)
2. In the search bar, type "Google Drive API"
3. Click on "Google Drive API" in the results
4. Click the blue "Enable" button
5. Wait for it to be enabled (you'll be redirected to the API overview page)

## Step 3: Configure the OAuth Consent Screen
1. In the left sidebar, go to "APIs & Services" → "OAuth consent screen" (or go to https://console.cloud.google.com/apis/credentials/consent)
2. Select "External" as the User Type and click "Create"
3. Fill in the required fields:
   - App name: "CloudDrive"
   - User support email: select your email
   - Developer contact information: enter your email
4. Click "Save and Continue"
5. On the "Scopes" page, click "Add or Remove Scopes"
6. Search for "Google Drive API" and check the box for `.../auth/drive` (described as "See, edit, create, and delete all of your Google Drive files")
7. Click "Update" then "Save and Continue"
8. On the "Test users" page, click "Add Users"
9. Enter ALL the Gmail addresses you want to use with CloudDrive (up to 4). For example:
   - you@gmail.com
   - you.backup1@gmail.com
   - you.backup2@gmail.com
   - you.backup3@gmail.com
10. Click "Add" then "Save and Continue"
11. Review the summary and click "Back to Dashboard"

### Important: Understanding "Testing" Mode
Your app is now in "Testing" publishing status. This means:
- **Only the specific Gmail addresses you added as test users can log in.** Other accounts will see an error.
- **Refresh tokens expire after 7 days.** This means approximately once a week, CloudDrive will ask you to re-authenticate one or more accounts. This is a Google-imposed limitation for apps in Testing mode.
- **This is normal and expected.** Going through Google's formal app verification process is designed for apps serving many users, not personal tools. CloudDrive handles expired tokens gracefully — it will pause syncing for the affected account and prompt you to reconnect.

### Why "Full Drive Access" and not a narrower scope?
The `drive.file` scope only lets an app see files it created itself — not your existing files. Since CloudDrive needs to browse and manage ALL your files (including ones already in your Drive), it requires the broader `drive` scope. This scope is classified as "restricted" by Google, which is why the consent screen shows a warning. Since this is your own app accessing your own accounts, this is perfectly safe.

## Step 4: Create OAuth Client ID
1. In the left sidebar, go to "APIs & Services" → "Credentials" (or go to https://console.cloud.google.com/apis/credentials)
2. Click "+ Create Credentials" at the top
3. Select "OAuth client ID"
4. For "Application type", select "Desktop app"
5. Name it "CloudDrive Desktop" (or anything you like)
6. Click "Create"
7. A dialog will appear showing your **Client ID** and **Client Secret**
8. **Copy both values** — you'll need them in the next step
9. You can also click "Download JSON" to save them, but CloudDrive doesn't use the JSON file directly

## Step 5: Configure CloudDrive

### GUI App
1. Launch CloudDrive GUI: `./target/release/clouddrive-gui`
2. On first launch, you'll see a setup wizard
3. Paste your Client ID and Client Secret into the fields
4. Click "Save Credentials" — they are stored securely in your system's keyring (not in a plaintext file)
5. Click "Add Account" and follow the browser prompts to sign in with each Gmail account
6. Repeat for each account you want to add

### FUSE Mount
1. The FUSE mount shares credentials with the GUI — if you've already set them up in the GUI, they're ready
2. If not, configure credentials via the GUI first or place `credentials.json` directly into `~/.config/clouddrive/credentials.json` (see `credentials.example.json` in the root of the repository)
3. Mount your drive: `./target/release/clouddrive-fuse --mount-point ~/CloudDrive`

## Troubleshooting

### "Access blocked: CloudDrive has not completed the Google verification process"
This means the Google account you're trying to use wasn't added as a test user. Go back to Step 3 and add the email address.

### "This app is blocked" or similar errors
Make sure you're signed into the Google account that was added as a test user. Try using an incognito/private browser window.

### Re-authentication prompts
If CloudDrive asks you to reconnect an account, this is normal — Testing mode tokens expire after 7 days. Just click "Reconnect" and sign in again.

### "No keyring daemon available"
CloudDrive uses your system's secret storage. On Arch Linux:
- GNOME/XFCE: `sudo pacman -S gnome-keyring libsecret`
- KDE: `sudo pacman -S kwallet`
- i3/Sway/Hyprland: `sudo pacman -S gnome-keyring libsecret` and add `eval $(gnome-keyring-daemon --start --components=secrets)` to your shell profile
