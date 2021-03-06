use std::{
	thread,
	sync::{
		Arc, Mutex, MutexGuard, Condvar,
		atomic::{ AtomicBool, Ordering }
	},
	time::{ Duration, Instant }
};


/// A future's state
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
	/// The future is waiting to be set
	Waiting,
	/// The future has been set and is ready for consumption
	Ready,
	/// The future has been consumed
	Consumed,
	/// The future has been canceled
	Canceled
}


/// An inner state object for a future
struct Inner<T, U> {
	payload: Mutex<(State, Option<T>)>,
	cond_var: Condvar,
	shared_state: Mutex<U>,
	cancel_on_drop: AtomicBool
}
unsafe impl<T, U> Sync for Inner<T, U> {}


/// A future result with an optional shared state
pub struct Future<T, U = ()>(Arc<Inner<T, U>>);
impl<T, U> Future<T, U> {
	/// Creates a new `Future<T, U>` with `shared_state` as shared-state
	pub fn with_state(shared_state: U) -> Self {
		Future(Arc::new(Inner {
			payload: Mutex::new((State::Waiting, None)),
			cond_var: Condvar::new(),
			shared_state: Mutex::new(shared_state),
			cancel_on_drop: AtomicBool::new(true)
		}))
	}
	
	/// Sets the future
	pub fn set(&self, result: T) -> Result<(), State> {
		// Check if the future can be set (is `State::Waiting`)
		let mut payload = self.0.payload.lock().unwrap();
		if payload.0 != State::Waiting {
			Err(payload.0)?
		}
		
		// Set result
		*payload = (State::Ready, Some(result));
		self.0.cond_var.notify_all();
		Ok(())
	}
	/// Cancels (poisons) the future
	///
	/// This is useful to indicate that the future is obsolete and should not be `set` anymore
	pub fn cancel(&self) {
		let mut payload = self.0.payload.lock().unwrap();
		// Check if the payload is still cancelable
		if payload.0 == State::Waiting {
			payload.0 = State::Canceled;
			self.0.cond_var.notify_all();
		}
	}
	/// Returns the future's state
	pub fn get_state(&self) -> State {
		self.0.payload.lock().unwrap().0
	}
	/// Checks if the future is still waiting or has been set/canceled
	pub fn is_waiting(&self) -> bool {
		self.get_state() == State::Waiting
	}
	
	/// Tries to get the future's result
	///
	/// If the future is ready, it is consumed and `T` is returned;
	/// if the future is not ready, `Error::InvalidState(State)` is returned
	pub fn try_get(&self) -> Result<T, State> {
		// Lock this future and check if it has a result (is `State::Ready`)
		let payload = self.0.payload.lock().unwrap();
		Self::extract_payload(payload)
	}
	/// Tries to get the future's result
	///
	/// If the future is ready or or becomes ready before the timeout occurres, it is consumed
	/// and `T` is returned; if the future is not ready, `Error::InvalidState(State)` is returned
	pub fn try_get_timeout(&self, timeout: Duration) -> Result<T, State> {
		let timeout_point = Instant::now() + timeout;
		
		// Wait for condvar until the state is not `State::Waiting` anymore or the timeout has occurred
		let mut payload = self.0.payload.lock().unwrap();
		while payload.0 == State::Waiting && Instant::now() < timeout_point {
			payload = self.0.cond_var.wait_timeout(payload, time_remaining(timeout_point)).unwrap().0;
		}
		Self::extract_payload(payload)
	}
	/// Gets the future's result
	///
	/// __Warning: this function will block until a result becomes available__
	pub fn get(&self) -> Result<T, State> {
		// Wait for condvar until the state is not `State::Waiting` anymore
		let mut payload = self.0.payload.lock().unwrap();
		while payload.0 == State::Waiting {
			payload = self.0.cond_var.wait(payload).unwrap()
		}
		Self::extract_payload(payload)
	}

	/// Get a clone of the current shared state
	pub fn get_shared_state(&self) -> U where U: Clone {
		self.0.shared_state.lock().unwrap().clone()
	}
	/// Replace the current shared state
	pub fn set_shared_state(&self, shared_state: U) {
		*self.0.shared_state.lock().unwrap() = shared_state
	}
	
	/// Provides exclusive access to the shared state within `modifier` until `modifier` returns
	pub fn access_shared_state<F: FnOnce(&mut U)>(&self, modifier: F) {
		let mut shared_state_lock = self.0.shared_state.lock().unwrap();
		modifier(&mut *shared_state_lock);
	}
	/// Provides exclusive access to the shared state within `modifier` until `modifier` returns
	pub fn access_shared_state_param<V, F: FnOnce(&mut U, V)>(&self, modifier: F, parameter: V) {
		let mut shared_state_lock = self.0.shared_state.lock().unwrap();
		modifier(&mut *shared_state_lock, parameter);
	}
	
	/// Detaches the future so it won't be canceled if there is only one instance left
	///
	/// Useful if you either don't want that your future is ever canceled or if there's always only
	/// one instance (e.g. if you wrap it into a reference-counting container)
	pub fn detach(&self) {
		self.0.cancel_on_drop.store(false, Ordering::Relaxed)
	}
	
	/// Internal helper to validate/update the future's state and get the payload
	fn extract_payload(mut payload: MutexGuard<(State, Option<T>)>) -> Result<T, State> {
		// Validate state
		if payload.0 == State::Ready {
			payload.0 = State::Consumed;
			if let Some(payload) = payload.1.take() {
				return Ok(payload)
			}
		}
		Err(payload.0)?
	}
}
impl<T> Future<T, ()> {
	pub fn new() -> Self {
		Future::with_state(())
	}
}
impl<T, U> Default for Future<T, U> where U: Default {
	fn default() -> Self {
		Future::with_state(U::default())
	}
}
impl<T, U> Drop for Future<T, U> {
	fn drop(&mut self) {
		if Arc::strong_count(&self.0) <= 2 && self.0.cancel_on_drop.load(Ordering::Relaxed) {
			self.cancel()
		}
	}
}
impl<T, U> Clone for Future<T, U> {
	fn clone(&self) -> Self {
		Future(self.0.clone())
	}
}
unsafe impl<T: Send, U: Send> Send for Future<T, U> {}
unsafe impl<T, U> Sync for Future<T, U> {}


/// Computes the remaining time underflow-safe
pub fn time_remaining(timeout_point: Instant) -> Duration {
	match Instant::now() {
		now if now > timeout_point => Duration::default(),
		now => timeout_point - now
	}
}


/// Creates a future for `job` and runs `job`. The result of `job` will be set as result into the
/// future. The parameter passed to `job` is a function that returns if the future is still waiting
/// so that `job` can check for cancellation.
pub fn run_async_with_state<T, U, F>(job: F, shared_state: U) -> Future<T, U>
	where T: 'static + Send, U: 'static + Send, F: FnOnce(Future<T, U>) + Send + 'static
{
	// Create future and spawn job
	let future = Future::with_state(shared_state);
	let _future = future.clone();
	thread::spawn(move || job(_future));
	
	future
}

/// Creates a future for `job` and runs `job`. The result of `job` will be set as result into the
/// future. The parameter passed to `job` is a function that returns if the future is still waiting
/// so that `job` can check for cancellation.
pub fn run_async<T, F>(job: F) -> Future<T, ()>
	where T: 'static + Send, F: FnOnce(Future<T, ()>) + Send + 'static
{
	run_async_with_state(job, ())
}


/// Sets `$result` as the `$future`'s result and returns
#[macro_export]
macro_rules! job_return {
    ($future:expr, $result:expr) => ({
    	let _ = $future.set($result);
		return
	})
}
/// Cancels `$future` and returns
#[macro_export]
macro_rules! job_die {
    ($future:expr) => ({
    	$future.cancel();
    	return
    })
}


#[cfg(test)]
mod test {
	use super::*;
	
	#[test]
	fn double_set_err() {
		let fut = Future::<u8>::new();
		fut.set(7).unwrap();
		assert_eq!(fut.set(77).unwrap_err(), State::Ready)
	}
	
	#[test]
	fn cancel_set_err() {
		let fut = Future::<u8>::new();
		fut.cancel();
		assert_eq!(fut.set(7).unwrap_err(), State::Canceled)
	}
	
	#[test]
	fn drop_is_canceled() {
		let fut = Future::<u8>::new();
		assert_eq!(fut.get_state(), State::Waiting);
		{
			let _fut = fut.clone();
			thread::sleep(Duration::from_secs(2));
		}
		assert_eq!(fut.get_state(), State::Canceled)
	}
	
	#[test]
	fn cancel_get_err() {
		let fut = run_async(|fut: Future<u8>| {
			thread::sleep(Duration::from_secs(4));
			job_die!(fut)
		});
		assert_eq!(fut.get().unwrap_err(), State::Canceled)
	}
	
	#[test]
	fn is_ready_and_get() {
		let fut = run_async(|fut: Future<u8>| {
			thread::sleep(Duration::from_secs(4));
			fut.set(7).unwrap();
		});
		assert_eq!(fut.get_state(), State::Waiting);
		
		// Create and drop future
		{
			let _fut = fut.clone();
			thread::sleep(Duration::from_secs(7));
			assert_eq!(_fut.get_state(), State::Ready);
		}
		
		assert_eq!(fut.get().unwrap(), 7);
	}
}