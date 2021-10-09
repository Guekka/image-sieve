extern crate item_sort_list;
extern crate rfd;
extern crate sixtyfps;

use num_traits::{FromPrimitive, ToPrimitive};
use rfd::FileDialog;
use sixtyfps::{Model, ModelHandle, SharedString};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Mutex;
use std::thread;
use std::{cell::RefCell, sync::Arc};

use crate::image_cache::ImageCache;
use crate::json_persistence::JsonPersistence;
use crate::json_persistence::{get_project_filename, get_settings_filename};
use crate::settings::Settings;
use crate::synchronize::Synchronizer;
use item_sort_list::{CommitMethod, ItemList};

sixtyfps::include_modules!();

type ImagesModelMap = HashMap<usize, usize>;

/// Main window container of the image sorter, contains the sixtyfps window, models and internal data structures
pub struct MainWindow {
    window: ImageSieve,
    item_list: Arc<Mutex<ItemList>>,
    item_list_model: Rc<sixtyfps::VecModel<SharedString>>,
    similar_items_model: Rc<sixtyfps::VecModel<SortImage>>,
    items_model_map: Rc<RefCell<ImagesModelMap>>,
    events_model: Rc<sixtyfps::VecModel<Event>>,
    image_cache: Rc<ImageCache>,
}

impl MainWindow {
    /// Creates a new main window and initializes it from saved settings
    pub fn new() -> Self {
        // Load settings and item list
        let settings: Settings =
            JsonPersistence::load(get_settings_filename()).unwrap_or_else(|| Settings::new());

        let item_list = ItemList {
            items: vec![],
            events: vec![],
            path: String::new(),
        };

        let item_list = Arc::new(Mutex::new(item_list));

        let event_list_model = Rc::new(sixtyfps::VecModel::<Event>::default());
        let item_list_model = Rc::new(sixtyfps::VecModel::<SharedString>::default());

        // Construct main window
        let image_sieve = ImageSieve::new();
        let synchronizer = Synchronizer::new(item_list.clone(), &image_sieve);

        // Start synchronization in a background thread
        synchronizer.synchronize(&settings.source_directory);

        let main_window = Self {
            window: image_sieve,
            item_list: item_list,
            item_list_model: item_list_model,
            similar_items_model: Rc::new(sixtyfps::VecModel::<SortImage>::default()),
            items_model_map: Rc::new(RefCell::new(HashMap::new())),
            events_model: event_list_model,
            image_cache: Rc::new(ImageCache::new()),
        };

        // Set initial values
        let version = env!("CARGO_PKG_VERSION");
        main_window
            .window
            .set_window_title(SharedString::from("ImageSieve v") + version);
        main_window
            .window
            .set_source_directory(SharedString::from(settings.source_directory));
        main_window
            .window
            .set_target_directory(SharedString::from(settings.target_directory));
        let commit_index = ToPrimitive::to_i32(&settings.commit_method).unwrap();
        main_window.window.set_commit_method(commit_index);
        let values: ModelHandle<SharedString> = main_window
            .window
            .global::<CommitMethodValues>()
            .get_values();
        main_window
            .window
            .set_commit_method_value(values.row_data(commit_index as usize));

        // Set model references
        main_window
            .window
            .set_images_list_model(sixtyfps::ModelHandle::new(
                main_window.item_list_model.clone(),
            ));
        main_window
            .window
            .set_images_model(sixtyfps::ModelHandle::new(
                main_window.similar_items_model.clone(),
            ));
        main_window
            .window
            .set_events_model(sixtyfps::ModelHandle::new(main_window.events_model.clone()));

        main_window.setup_callbacks();

        main_window
    }

    /// Start the event loop
    pub fn run(&self) {
        self.window.run();

        // Save settings when program exits
        let settings = Settings {
            source_directory: self.window.get_source_directory().to_string(),
            target_directory: self.window.get_target_directory().to_string(),
            commit_method: FromPrimitive::from_i32(self.window.get_commit_method())
                .unwrap_or_else(|| CommitMethod::Copy),
        };
        JsonPersistence::save(get_settings_filename(), &settings);

        // and save item list
        let item_list = self.item_list.lock().unwrap();
        JsonPersistence::save(
            &get_project_filename(item_list.path.as_str()),
            &item_list.clone(),
        );
    }

    /// Setup sixtyfps GUI callbacks
    fn setup_callbacks(&self) {
        self.window.on_item_selected({
            // New item selected on the list of images or next/previous clicked
            let item_list = self.item_list.clone();
            let similar_items_model = self.similar_items_model.clone();
            let items_model_map = self.items_model_map.clone();
            let window_weak = self.window.as_weak();
            let image_cache = self.image_cache.clone();

            move |i: i32| {
                synchronize_images_model(
                    i as usize,
                    &item_list.lock().unwrap(),
                    similar_items_model.clone(),
                    &mut items_model_map.borrow_mut(),
                    &window_weak,
                    &image_cache,
                );
            }
        });

        self.window.on_commit({
            // Commit pressed - perform selected action
            let window_weak = self.window.as_weak();
            let item_list = self.item_list.clone();

            move || {
                commit(&item_list.lock().unwrap(), window_weak.clone());
            }
        });

        self.window.on_take_over_toggle({
            // Image was clicked, toggle take over state
            let item_list_model = self.item_list_model.clone();
            let item_list = self.item_list.clone();
            let items_model = self.similar_items_model.clone();
            let items_model_map = self.items_model_map.clone();

            move |i: i32| {
                let model_index = i as usize;
                // Change the state of the SortImage in the items_model
                let mut sort_image = items_model.row_data(model_index);
                sort_image.take_over = !sort_image.take_over;

                let index = items_model_map.borrow_mut()[&model_index];
                {
                    // Change the item_list state
                    let mut item_list_mut = item_list.lock().unwrap();
                    item_list_mut.items[index].set_take_over(sort_image.take_over);
                }
                items_model.set_row_data(model_index, sort_image);
                // Update item list model to reflect change in icons in list
                synchronize_item_list_model(&item_list.lock().unwrap(), &item_list_model);
            }
        });

        self.window.on_browse_source({
            // Browse source was clicked, select new path
            let item_list_model = self.item_list_model.clone();
            let item_list = self.item_list.clone();
            let window_weak = self.window.as_weak();
            let synchronizer = Synchronizer::new(self.item_list.clone(), &self.window);

            move || {
                let file_dialog = FileDialog::new();
                match file_dialog.pick_folder() {
                    Some(folder) => {
                        {
                            // Save current item list
                            let item_list = item_list.lock().unwrap();
                            if item_list.items.len() > 0 {
                                JsonPersistence::save(
                                    &get_project_filename(item_list.path.as_str()),
                                    &item_list.clone(),
                                );
                            }
                        }

                        let source_path = folder.to_str().unwrap();
                        empty_model(item_list_model.clone());

                        // Synchronize in a background thread
                        window_weak.unwrap().set_loading(true);
                        synchronizer.synchronize(source_path);

                        window_weak
                            .unwrap()
                            .set_source_directory(SharedString::from(source_path));
                    }
                    None => {}
                }
            }
        });

        self.window.on_browse_target({
            // Commit target path was changed
            let window_weak = self.window.as_weak();

            move || {
                let file_dialog = FileDialog::new();
                match file_dialog.pick_folder() {
                    Some(folder) => {
                        let target_path = &String::from(folder.to_str().unwrap());

                        window_weak
                            .unwrap()
                            .set_target_directory(SharedString::from(target_path));
                    }
                    None => {}
                }
            }
        });

        self.window.on_add_event({
            // New event was added, return true if the dates are ok
            let item_list_model = self.item_list_model.clone();
            let events_model = self.events_model.clone();
            let item_list = self.item_list.clone();

            move |name, start_date: SharedString, end_date: SharedString| -> bool {
                let name_s = name.clone().to_string();
                let event =
                    item_sort_list::Event::new(name_s, start_date.as_str(), end_date.as_str());
                if event.is_ok() {
                    events_model.push(Event {
                        name,
                        start_date,
                        end_date,
                    });
                    let mut item_list = item_list.lock().unwrap();
                    item_list.events.push(event.unwrap());
                    // Synchronize the item list to update the icons of the entries
                    synchronize_item_list_model(&item_list, &item_list_model.clone());
                    true
                } else {
                    false
                }
            }
        });

        self.window
            .on_date_valid(|date: sixtyfps::SharedString| -> bool {
                item_sort_list::Event::is_date_valid(date.to_string().as_str())
            });

        self.window.on_remove_event({
            // Event was removed
            let item_list_model = self.item_list_model.clone();
            let events_model = self.events_model.clone();
            let item_list = self.item_list.clone();

            move |index| {
                events_model.remove(index as usize);
                let mut item_list = item_list.lock().unwrap();
                item_list.events.remove(index as usize);
                // Synchronize the item list to update the icons of the entries
                synchronize_item_list_model(&item_list, &item_list_model.clone());
            }
        });
    }
}

fn empty_model(item_list_model: Rc<sixtyfps::VecModel<SharedString>>) {
    for _ in 0..item_list_model.row_count() {
        item_list_model.remove(0);
    }
}

/// Synchronizes the list of found items from the internal data structure with the sixtyfps VecModel
pub fn synchronize_item_list_model(
    item_list: &ItemList,
    item_list_model: &sixtyfps::VecModel<SharedString>,
) {
    let empty_model = item_list_model.row_count() == 0;
    for (index, image) in item_list.items.iter().enumerate() {
        let mut item_string = image.get_item_string(&item_list.path);
        if item_list.get_event(image).is_some() {
            item_string = String::from("\u{1F4C5}") + &item_string;
        }
        if empty_model {
            item_list_model.push(SharedString::from(item_string));
        } else {
            item_list_model.set_row_data(index, SharedString::from(item_string));
        }
    }
}

/// Synchronizes the images to show at the same time from a selected image to the sixtyfps VecModel
fn synchronize_images_model(
    selected_item_index: usize,
    item_list: &ItemList,
    similar_items_model: Rc<sixtyfps::VecModel<SortImage>>,
    item_model_map: &mut ImagesModelMap,
    window: &sixtyfps::Weak<ImageSieve>,
    image_cache: &ImageCache,
) {
    let similars = item_list.items[selected_item_index].get_similars();

    // Clear images model and the model map
    for _ in 0..similar_items_model.row_count() {
        similar_items_model.remove(0);
    }
    item_model_map.drain();

    let mut model_index: usize = 0;

    let mut add_item = |item_index: &usize| {
        let item = &item_list.items[*item_index];
        let image = image_cache.load(item);

        let sort_image_struct = SortImage {
            image: image,
            take_over: item.get_take_over(),
        };
        similar_items_model.push(sort_image_struct);
        item_model_map.insert(model_index, *item_index);
        model_index += 1;
    };

    add_item(&selected_item_index);

    for image_index in similars {
        add_item(image_index);
    }

    // Prefetch next two images
    let mut prefetch_index = selected_item_index + 1;
    let mut prefetches = 2;
    while prefetches > 0 && prefetch_index < item_list.items.len() {
        if !similars.contains(&prefetch_index) {
            if let Some(file_item) = item_list.items.get(prefetch_index) {
                if file_item.is_image() {
                    image_cache.prefetch(file_item);
                    prefetches -= 1;
                }
            }
        }
        prefetch_index += 1;
    }

    // Set properties
    window
        .unwrap()
        .set_current_image(similar_items_model.row_data(0));
    window.unwrap().set_current_image_index(0);
    window
        .unwrap()
        .set_num_images(similar_items_model.row_count() as i32);

    let item = &item_list.items[selected_item_index];
    let mut item_text = item.get_item_string(&String::from(""));
    let item_size = item.get_size() / 1024;
    let item_date = item.get_date_str();
    let event = item_list.get_event(item);
    let event_str = if let Some(event) = event {
        event.name.as_str()
    } else {
        ""
    };
    item_text += format!(" - {}, {} KB {}", item_date, item_size, event_str).as_str();
    window
        .unwrap()
        .set_current_image_text(SharedString::from(item_text));
}

pub fn commit(item_list: &ItemList, window_weak: sixtyfps::Weak<ImageSieve>) {
    let item_list_copy = item_list.to_owned();
    let target_path = window_weak.unwrap().get_target_directory().to_string();
    let commit_method = FromPrimitive::from_i32(window_weak.unwrap().get_commit_method())
        .unwrap_or_else(|| CommitMethod::Copy);

    thread::spawn(move || {
        let progress_callback = |progress: String| {
            let window_weak_copy = window_weak.clone();
            window_weak_copy.upgrade_in_event_loop(move |handle| {
                if progress == "Done" {
                    handle.set_commit_running(false);
                }
                handle.set_commit_message(SharedString::from(progress));
            });
        };
        item_list_copy.commit(&target_path, commit_method, progress_callback);
    });
}
