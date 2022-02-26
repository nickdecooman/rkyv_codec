#![feature(associated_type_bounds)]

use std::{ops::Range, pin::Pin, task::{Context, Poll}};

#[macro_use]
extern crate thiserror;
#[macro_use]
extern crate pin_project;

use bytecheck::CheckBytes;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, Sink};
use rkyv::{AlignedVec, Archive, Archived, Serialize, ser::{Serializer, serializers::{AllocScratch, CompositeSerializer, FallbackScratch, HeapScratch, SharedSerializeMap, WriteSerializer}}, validation::validators::DefaultValidator};

#[derive(Debug, Error)]
pub enum PacketCodecError {
	#[error(transparent)]
	IoError(#[from] futures::io::Error),
	#[error("Packet not correctly archived")]
	CheckArchiveError,
	#[error("Failed to Serialize")]
	SerializeError,
	#[error("Failed to parse length")]
	ReadError(#[from] unsigned_varint::io::ReadError),
	#[error("Premature End of File Error")]
	EOFError,
}

macro_rules! ready {
	($e:expr $(,)?) => {
		match $e {
			$crate::Poll::Ready(t) => t,
			$crate::Poll::Pending => return $crate::Poll::Pending,
		}
	};
}

/// Reads a single Archived Packet into as passed buffer from an AsyncRead
/// This is currently the fastest method I could come up with for streaming rkyv that requires no recurring heap allocations
pub async fn stream<'b, Inner: AsyncRead + Unpin, Packet>(mut inner: &mut Inner, buffer: &'b mut AlignedVec) -> Result<&'b Archived<Packet>, PacketCodecError>
where
	Packet: rkyv::Archive<Archived: CheckBytes<DefaultValidator<'b>> + 'b>,
{
	let archive_len = unsigned_varint::aio::read_usize(&mut inner).await?;
	buffer.reserve(archive_len - buffer.len()); // Reserve at least the amount of bytes needed
	unsafe { buffer.set_len(archive_len); } // Safety: Already reserved the required space

	inner.read_exact(buffer).await?;
	let archive = rkyv::check_archived_root::<'b, Packet>(buffer).map_err(|_|PacketCodecError::CheckArchiveError)?;
	Ok(archive)
}

/// This structure wraps an AsyncWrite and implements Sink to serialize and write Archive objects.
#[pin_project]
pub struct RkyvWriter<Writer: AsyncWrite> {
	#[pin] writer: Writer,
	buffer: AlignedVec,
	length_buffer: [u8; 10],
	len_state: Range<usize>, // How much of the length buffer has been written
	buf_state: usize, // Whether or not the aligned buf is being written and if so, how much so far
}
impl<Writer: AsyncWrite> RkyvWriter<Writer> {
	pub fn new(writer: Writer) -> Self { Self { writer, buffer: AlignedVec::new(), length_buffer: [0u8; 10], len_state: Default::default(), buf_state: 0, } }
	pub fn inner(self) -> Writer { self.writer }
}

impl<Writer: AsyncWrite, Packet> Sink<Packet> for RkyvWriter<Writer>
where
	Packet: Archive + for<'b> Serialize<CompositeSerializer<WriteSerializer<&'b mut AlignedVec>, FallbackScratch<HeapScratch<0>, AllocScratch>, SharedSerializeMap>>,
{
	type Error = PacketCodecError;

	fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.project().writer.poll_flush(cx).map_err(|e|PacketCodecError::IoError(e))
	}

	fn start_send(self: Pin<&mut Self>, item: Packet) -> Result<(), Self::Error> {
		let this = self.project();
		this.buffer.clear();
		let serializer = WriteSerializer::new(this.buffer);
		let mut serializer = CompositeSerializer::new(serializer, FallbackScratch::default(), SharedSerializeMap::default());
		let _bytes_written = serializer.serialize_value(&item).map_err(|_|PacketCodecError::SerializeError)?;
		
		let bytes_written = serializer.into_serializer().into_inner().len();
		*this.len_state = 0..unsigned_varint::encode::usize(bytes_written, this.length_buffer).len();
		*this.buf_state = 0;
		Ok(())
	}

	fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		let mut this = self.project();

		let len_state = this.len_state;
		if len_state.start <= len_state.end {
			let length_buffer = &this.length_buffer[len_state.clone()];
			
			let written = ready!(Pin::new(&mut this.writer).poll_write(cx, length_buffer)?);
			len_state.start += written;
		}
		
		while *this.buf_state < this.buffer.len() {
			let buffer_left = &this.buffer[*this.buf_state..this.buffer.len()];
			let bytes_written = ready!(Pin::new(&mut this.writer).poll_write(cx, buffer_left))?;
			if bytes_written == 0 { return Poll::Ready(Err(PacketCodecError::EOFError)) }
			*this.buf_state += bytes_written;
		}

		ready!(this.writer.poll_flush(cx)?);
		Poll::Ready(Ok(()))
	}

	fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.project().writer.poll_close(cx).map_err(|e|PacketCodecError::IoError(e))
	}
}

#[cfg(test)]
mod test {
	use rkyv::{AlignedVec, Archive, Archived, Deserialize, Infallible, Serialize};
	use bytecheck::CheckBytes;
	use futures::SinkExt;
	
	use crate::{RkyvWriter, stream};

	#[async_std::test]
	async fn test_local() {
		#[derive(Archive, Deserialize, Serialize, Debug, PartialEq, Clone)]
		// This will generate a PartialEq impl between our unarchived and archived types
		#[archive(compare(PartialEq))]
		// To use the safe API, you have to derive CheckBytes for the archived type
		#[archive_attr(derive(CheckBytes, Debug))]
		struct Test {
			int: u8,
			string: String,
			option: Option<Vec<i32>>,
		}
		let value = Test {
			int: 42,
			string: "hello world".to_string(),
			option: Some(vec![1, 2, 3, 4]),
		};
		println!("starting test");
		
		let writer = Vec::new();
		let mut codec = RkyvWriter::new(writer);
		codec.send(value.clone()).await.unwrap();

		let mut reader = &codec.inner()[..];

		println!("serialized data: {:?}", reader);
		let mut buffer = AlignedVec::new();
		let data: &Archived<Test> = stream::<_, Test>(&mut reader, &mut buffer).await.unwrap();
		
		let value_sent: Test = data.deserialize(&mut Infallible).unwrap();

		assert_eq!(value, value_sent);
	}
}