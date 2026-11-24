mod ssh;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(ssh::AppState::default())
        .invoke_handler(tauri::generate_handler![
            ssh::ssh_connect,
            ssh::ssh_disconnect,
            ssh::ssh_reconnect,
            ssh::ssh_alive,
            ssh::local_open,
            ssh::secret_set,
            ssh::secret_get,
            ssh::secret_delete,
            ssh::term_open,
            ssh::term_write,
            ssh::term_resize,
            ssh::term_close,
            ssh::sftp_home,
            ssh::sftp_realpath,
            ssh::sftp_list,
            ssh::sftp_mkdir,
            ssh::sftp_rename,
            ssh::sftp_remove,
            ssh::sftp_download,
            ssh::sftp_upload,
            ssh::profile_list,
            ssh::profile_save,
            ssh::profile_delete,
            ssh::default_download_dir,
            ssh::sys_stats,
            ssh::sftp_find,
            ssh::sftp_stat,
            ssh::sys_procs,
            ssh::proc_kill,
            ssh::sftp_copy,
            ssh::ssh_config_hosts,
            ssh::sftp_read_text,
            ssh::sftp_write_text,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
