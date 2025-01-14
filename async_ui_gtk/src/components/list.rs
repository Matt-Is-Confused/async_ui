use std::{future::IntoFuture, marker::PhantomData, rc::Rc};

use async_task::Task;
pub use async_ui_core::list::ListModel;
use async_ui_core::{
    backend::BackendTrait,
    executor::spawn_local,
    list::{Change, ListModelPrivateAPIs},
    vnode::{
        node_concrete::{ConcreteNodeVNode, RefNode},
        VNodeTrait, WithVNode,
    },
};
use futures_lite::pin;
use glib::Cast;
use gtk::{traits::BoxExt, Widget};
use im_rc::Vector;
use observables::{Listenable, ObservableAs, ObservableAsExt};
use scoped_async_spawn::SpawnGuard;
use slab::Slab;

use crate::{
    backend::Backend,
    widget::{gtk_box::GtkBoxOp, WidgetOp, WrappedWidget},
};

use super::ElementFuture;

pub struct ListProps<'c, T: Clone, F: IntoFuture> {
    pub data: &'c dyn ObservableAs<ListModel<T>>,
    pub render: &'c dyn Fn(T) -> F,
}

struct DummyObservableAs<T>(PhantomData<T>);
const DUMMY_USED: &str = "dummy prop used";
impl<T: Clone> Listenable for DummyObservableAs<T> {
    fn add_waker(&self, _waker: std::task::Waker) {
        panic!("{}", DUMMY_USED)
    }
    fn get_version(&self) -> observables::Version {
        panic!("{}", DUMMY_USED)
    }
}
impl<T: Clone> ObservableAs<ListModel<T>> for DummyObservableAs<T> {
    fn visit_dyn_as(&self, _visitor: &mut dyn FnMut(&ListModel<T>)) {
        panic!("{}", DUMMY_USED)
    }
}
impl<'c, T: Clone + 'c, F: IntoFuture> Default for ListProps<'c, T, F> {
    fn default() -> Self {
        Self {
            data: &DummyObservableAs(PhantomData),
            render: &|_: T| panic!("{}", DUMMY_USED),
        }
    }
}

pub async fn list<'c, T: Clone, F: IntoFuture>(ListProps { data, render }: ListProps<'c, T, F>) {
    let container_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let container_node: gtk::Widget = container_box.clone().upcast();
    let container_node_copy = container_node.clone();
    let wrapped_container_node = WrappedWidget {
        widget: container_node.clone(),
        inner_widget: container_node.clone().upcast(),
        op: WidgetOp::MultiChild(&GtkBoxOp),
    };
    let inside = async move {
        let parent_vnode = Backend::get_vnode_key().with(Clone::clone);

        let parent_context = parent_vnode.get_context_map();
        let mut tasks: Slab<Task<()>> = Slab::new();
        let guard = SpawnGuard::new();
        pin!(guard);
        let mut nodes: Vector<(Widget, usize)> = Vector::new();
        let mut create_item_task = |fut: F::IntoFuture, after_this: Option<&Widget>| {
            let reference_node: Widget = gtk::Separator::new(gtk::Orientation::Horizontal).upcast();
            use gtk::traits::WidgetExt;
            reference_node.insert_after(&container_node, after_this);
            let fut = {
                WithVNode::new(
                    fut,
                    Rc::new(
                        ConcreteNodeVNode::new(
                            RefNode::<Backend>::Sibling {
                                parent: wrapped_container_node.clone(),
                                sibling: WrappedWidget {
                                    widget: reference_node.clone(),
                                    inner_widget: reference_node.clone().upcast(),
                                    op: WidgetOp::NoChild,
                                },
                            },
                            parent_context.clone(),
                        )
                        .into(),
                    ),
                )
            };
            let fut = guard.as_mut().convert_future(async {
                fut.await;
            });
            let task = spawn_local(fut);
            (reference_node, task)
        };
        let mut last_version = data.visit(|model| {
            let start = model.underlying_vector();
            let mut last_node = None;
            for item in start.iter() {
                let fut = render(item.to_owned()).into_future();
                let (node, task) = create_item_task(fut, last_node.as_ref());
                last_node = Some(node.to_owned());
                let task_id = tasks.insert(task);
                nodes.push_back((node, task_id));
            }
            let model = ListModelPrivateAPIs(model);
            model
                .total_listeners()
                .set(model.total_listeners().get() + 1);
            model.get_version()
        });
        let _guard = scopeguard::guard((), |_| {
            data.visit(|model| {
                let model = ListModelPrivateAPIs(&*model);
                model
                    .total_listeners()
                    .set(model.total_listeners().get() - 1);
            });
        });
        let mut first = true;
        data.for_each(|model| {
            if first {
                first = false;
                return;
            }
            let model_priv = ListModelPrivateAPIs(model);
            let changes = model_priv.changes_since_version(last_version);
            for change in changes {
                match change {
                    Change::Splice {
                        remove_range,
                        replace_with,
                    } => {
                        let n_items = ExactSizeIterator::len(remove_range);
                        let mut right = nodes.split_off(remove_range.start);
                        let new_right = right.split_off(n_items);
                        for (node, task_id) in right.into_iter() {
                            std::mem::drop(tasks.remove(task_id));
                            container_box.remove(&node);
                        }
                        let insert_after: Option<Widget> =
                            nodes.back().map(|(node, _)| node).cloned();
                        nodes.extend(replace_with.iter().map(|t| {
                            let fut = render(t.to_owned()).into_future();
                            let (node, task) = create_item_task(fut, insert_after.as_ref());
                            let task_id = tasks.insert(task);
                            (node, task_id)
                        }));
                        nodes.append(new_right);
                    }
                    Change::Remove { index } => {
                        let (node, task_id) = nodes.remove(*index);
                        std::mem::drop(tasks.remove(task_id));
                        container_box.remove(&node);
                    }
                    Change::Insert { index, value } => {
                        let fut = render(value.to_owned()).into_future();
                        let (node, task) = create_item_task(fut, {
                            (*index > 0)
                                .then(|| nodes.get(index - 1).map(|(node, _task_id)| node))
                                .flatten()
                        });
                        let task_id = tasks.insert(task);
                        nodes.insert(*index, (node, task_id));
                    }
                }
            }
            last_version = model_priv.get_version();
            model_priv
                .pending_listeners()
                .set(model_priv.pending_listeners().get() - 1);
        })
        .await
    };
    let scroll_window = gtk::ScrolledWindow::new();
    scroll_window.set_child(Some(&container_node_copy));
    scroll_window.set_propagate_natural_height(true);
    scroll_window.set_propagate_natural_width(true);
    ElementFuture::new(
        inside,
        WrappedWidget {
            widget: scroll_window.clone().upcast(),
            inner_widget: scroll_window.upcast(),
            op: WidgetOp::NoChild,
        },
    )
    .await;
}
