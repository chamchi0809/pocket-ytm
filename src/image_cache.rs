use std::{collections::HashMap, sync::Arc};

use futures::FutureExt as _;
use gpui::{
    AnyImageCache, App, AppContext as _, Asset as _, AssetLogger, Context, ElementId, Entity,
    ImageAssetLoader, ImageCache, ImageCacheError, ImageCacheItem, ImageCacheProvider, RenderImage,
    Resource, Window, hash,
};

pub fn bounded_image_cache(
    id: impl Into<ElementId>,
    max_items: usize,
) -> BoundedImageCacheProvider {
    BoundedImageCacheProvider {
        id: id.into(),
        max_items,
    }
}

pub struct BoundedImageCacheProvider {
    id: ElementId,
    max_items: usize,
}

impl ImageCacheProvider for BoundedImageCacheProvider {
    fn provide(&mut self, window: &mut Window, cx: &mut App) -> AnyImageCache {
        window
            .with_global_id(self.id.clone(), |global_id, window| {
                window.with_element_state::<Entity<BoundedImageCache>, _>(
                    global_id,
                    |cache, _window| {
                        let cache =
                            cache.filter(|cache| cache.read(cx).max_items == self.max_items);
                        let cache = cache.unwrap_or_else(|| {
                            cx.new(|cx| BoundedImageCache::new(self.max_items, cx))
                        });
                        (cache.clone(), cache)
                    },
                )
            })
            .into()
    }
}

struct BoundedImageCache {
    max_items: usize,
    usages: Vec<u64>,
    cache: HashMap<u64, ImageCacheItem>,
}

impl BoundedImageCache {
    fn new(max_items: usize, cx: &mut Context<Self>) -> Self {
        cx.on_release(|cache, cx| {
            for (_, mut item) in std::mem::take(&mut cache.cache) {
                if let Some(Ok(image)) = item.get() {
                    cx.drop_image(image, None);
                }
            }
        })
        .detach();

        Self {
            max_items,
            usages: Vec::with_capacity(max_items),
            cache: HashMap::with_capacity(max_items),
        }
    }

    fn mark_recent(&mut self, image_hash: u64) {
        if let Some(index) = self.usages.iter().position(|item| *item == image_hash) {
            self.usages.remove(index);
        }
        self.usages.insert(0, image_hash);
    }
}

impl ImageCache for BoundedImageCache {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let image_hash = hash(resource);
        if self.cache.contains_key(&image_hash) {
            self.mark_recent(image_hash);
            return self
                .cache
                .get_mut(&image_hash)
                .and_then(ImageCacheItem::get);
        }

        let future = AssetLogger::<ImageAssetLoader>::load(resource.clone(), cx);
        let task = cx.background_executor().spawn(future).shared();
        if self.usages.len() == self.max_items
            && let Some(oldest) = self.usages.pop()
            && let Some(mut item) = self.cache.remove(&oldest)
            && let Some(Ok(image)) = item.get()
        {
            cx.drop_image(image, Some(window));
        }
        self.cache
            .insert(image_hash, ImageCacheItem::Loading(task.clone()));
        self.mark_recent(image_hash);

        let entity = window.current_view();
        window
            .spawn(cx, async move |cx| {
                _ = task.await;
                cx.on_next_frame(move |_, cx| cx.notify(entity));
            })
            .detach();
        None
    }
}
