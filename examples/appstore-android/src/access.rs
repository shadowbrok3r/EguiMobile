use egui_mobile::egui;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone, Default, Deserialize)]
pub struct Group {
    pub id: i64,
    pub name: String,
    pub apps: Vec<String>,
}
#[derive(Clone, Default, Deserialize)]
pub struct Person {
    pub id: i64,
    pub username: String,
    pub is_admin: bool,
    pub all_apps: bool,
    pub groups: Vec<i64>,
}
#[derive(Clone, Default, Deserialize)]
pub struct Invite {
    pub id: i64,
    pub name: String,
    pub group_name: String,
    pub uses: i64,
    pub max_uses: i64,
    pub revoked: bool,
}
#[derive(Clone, Default, Deserialize)]
pub struct Snapshot {
    pub groups: Vec<Group>,
    pub users: Vec<Person>,
    pub invites: Vec<Invite>,
}
pub struct Command {
    pub path: String,
    pub body: Value,
}
#[derive(Default)]
pub struct AccessUi {
    pub snapshot: Snapshot,
    pub invite_link: String,
    group: Group,
    invite_name: String,
    invite_group: i64,
    days: i64,
    uses: i64,
    revoke: Option<String>,
}
impl AccessUi {
    pub fn show(&mut self, ui: &mut egui::Ui, apps: &[(String, String, bool)]) -> Option<Command> {
        let mut command = None;
        ui.heading("Sharing & access");
        ui.collapsing("Public apps", |ui| {
            ui.label("Anyone can install these apps without an invite.");
            for (slug, name, public) in apps {
                let mut value = *public;
                if ui.checkbox(&mut value, name).changed() {
                    command = Some(Command {
                        path: format!("/api/access/apps/{slug}"),
                        body: json!({"public":value}),
                    });
                }
            }
        });
        ui.collapsing("Private groups",|ui|{
            egui::ComboBox::from_id_salt("access_group").selected_text(if self.group.id==0{"New group"}else{&self.group.name}).show_ui(ui,|ui|{
                if ui.selectable_label(self.group.id==0,"New group").clicked(){self.group=Group::default();}
                for group in &self.snapshot.groups {if ui.selectable_label(self.group.id==group.id,&group.name).clicked(){self.group=group.clone();}}
            });
            ui.add(egui::TextEdit::singleline(&mut self.group.name).hint_text("Group name").desired_width(f32::INFINITY));
            for(slug,name,_)in apps {
                let mut selected=self.group.apps.contains(slug);
                if ui.checkbox(&mut selected,name).changed(){if selected{self.group.apps.push(slug.clone());}else{self.group.apps.retain(|s|s!=slug);}}
            }
            if ui.button("Save group").clicked(){command=Some(Command{path:"/api/access/groups".into(),body:json!({"id":if self.group.id==0{None}else{Some(self.group.id)},"name":self.group.name,"apps":self.group.apps})});}
        });
        ui.collapsing("Invites",|ui|{
            ui.add(egui::TextEdit::singleline(&mut self.invite_name).hint_text("Invitation name").desired_width(f32::INFINITY));
            if self.invite_group==0 {self.invite_group=self.snapshot.groups.first().map(|g|g.id).unwrap_or(0);}
            egui::ComboBox::from_id_salt("invite_group").selected_text(self.snapshot.groups.iter().find(|g|g.id==self.invite_group).map(|g|g.name.as_str()).unwrap_or("Choose group")).show_ui(ui,|ui|{
                for group in &self.snapshot.groups {ui.selectable_value(&mut self.invite_group,group.id,&group.name);}
            });
            if self.days==0{self.days=7;}if self.uses==0{self.uses=1;}
            ui.horizontal(|ui|{ui.label("Days");ui.add(egui::DragValue::new(&mut self.days).range(1..=90));ui.label("Uses");ui.add(egui::DragValue::new(&mut self.uses).range(1..=1000));});
            if ui.add_enabled(self.invite_group>0,egui::Button::new("Create invite")).clicked(){command=Some(Command{path:"/api/access/invites".into(),body:json!({"name":self.invite_name,"group_id":self.invite_group,"days":self.days,"max_uses":self.uses})});}
            if !self.invite_link.is_empty(){ui.label("Invite ready");if ui.button("Copy invite link").clicked(){ui.ctx().copy_text(self.invite_link.clone());}}
            ui.weak("Revoking a link stops new joins. Revoke a device below to remove existing access.");
            for invite in &self.snapshot.invites {
                ui.separator();ui.label(format!("{} · {} · {}/{} used",invite.name,invite.group_name,invite.uses,invite.max_uses));
                if !invite.revoked && ui.button(format!("Revoke invite #{}",invite.id)).clicked(){command=Some(Command{path:format!("/api/access/invites/{}/revoke",invite.id),body:json!({})});}
            }
        });
        ui.collapsing("People & devices", |ui| {
            for user in &mut self.snapshot.users {
                if user.is_admin {
                    continue;
                }
                ui.push_id(user.id, |ui| {
                    ui.collapsing(&user.username, |ui| {
                        ui.checkbox(&mut user.all_apps, "All apps, including future apps");
                        for group in &self.snapshot.groups {
                            let mut selected = user.groups.contains(&group.id);
                            if ui.checkbox(&mut selected, &group.name).changed() {
                                if selected {
                                    user.groups.push(group.id);
                                } else {
                                    user.groups.retain(|id| *id != group.id);
                                }
                            }
                        }
                        if ui.button("Save access").clicked() {
                            command = Some(Command {
                                path: format!("/api/access/users/{}", user.id),
                                body: json!({"all_apps":user.all_apps,"groups":user.groups}),
                            });
                        }
                        if self.revoke.as_deref() == Some(&user.username) {
                            ui.label("Revoke every credential for this device?");
                            ui.horizontal(|ui| {
                                if ui.button("Confirm revoke").clicked() {
                                    // User names may contain spaces and punctuation; encode a path segment.
                                    let mut url =
                                        reqwest::Url::parse("https://store.invalid/api/users/")
                                            .unwrap();
                                    url.path_segments_mut()
                                        .unwrap()
                                        .push(&user.username)
                                        .push("revoke");
                                    command = Some(Command {
                                        path: url.path().into(),
                                        body: json!({}),
                                    });
                                    self.revoke = None;
                                }
                                if ui.button("Cancel").clicked() {
                                    self.revoke = None;
                                }
                            });
                        } else if ui.button("Revoke device credentials").clicked() {
                            self.revoke = Some(user.username.clone());
                        }
                    })
                });
            }
        });
        command
    }
}

pub fn server_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let url = reqwest::Url::parse(raw).map_err(|_| "Use a complete HTTPS store address")?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "10.0.2.2"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Use HTTPS, without a password or query in the store address".into());
    }
    Ok(url.as_str().trim_end_matches('/').into())
}
pub fn invitation(raw: &str, current_server: &str) -> Result<(String, String), String> {
    let raw = raw.trim();
    let (base, code) = if raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        (server_url(current_server)?, raw.into())
    } else {
        let url = reqwest::Url::parse(raw).map_err(|_| "Paste an invitation link or code")?;
        if url.path() != "/join" {
            return Err("This is not an App Store invitation".into());
        }
        let code = url.fragment().unwrap_or("").to_owned();
        let mut base = url.clone();
        base.set_fragment(None);
        base.set_path("");
        (server_url(base.as_str())?, code)
    };
    if code.len() != 64 || !code.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Invalid invitation code".into());
    }
    Ok((base, code))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invites_keep_credentials_out_of_urls() {
        let code = "a".repeat(64);
        assert_eq!(
            invitation(&format!("https://store.test/join#{code}"), "").unwrap(),
            ("https://store.test".into(), code.clone())
        );
        assert_eq!(invitation(&code, "https://store.test").unwrap().1, code);
        for bad in [
            "http://store.test",
            "https://user:secret@store.test",
            "https://store.test?api_key=secret",
            "file:///etc/passwd",
        ] {
            assert!(server_url(bad).is_err(), "{bad}");
        }
        assert!(server_url("http://127.0.0.1:19879").is_ok());
        assert!(invitation("https://store.test/join#short", "").is_err());
    }
}
